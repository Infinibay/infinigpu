# 26 — Client-Side GPU Offload & Split Rendering: Delegating Work to the Client GPU

**Scope.** ADR-0009 designed infiniPixel as a host-heavy pipeline: the host renders the guest
desktop to a headless Vulkan context, NVENC/Vulkan-Video encodes it zero-readback, WebTransport
ships it, and a browser WebCodecs client *decodes and composites*. That client was treated as a
near-thin terminal. The owner's new insight corrects that: **the client is a full PC with its own
GPU** — even a minimal one has hardware video codecs, 2D/3D acceleration, and in-browser
WebGL/WebGPU/WebCodecs/WebNN. This doc designs the **client-offload / split-rendering layer**: what
work moves off the host GPU onto the client GPU, the host-side changes to enable each, and the
host-GPU / bandwidth / latency savings — **negotiated by client capability, with a thin-client
fallback to the ADR-0009 pixel path**. The through-line is ADR-0007's density goal: every joule of
host GPU-time we *don't* spend encoding a desktop is a joule available to pack another VM onto the
2× A5000.

## Verdict up front

Four offloads, ranked by density payoff and safety:

1. **Client-side composition + post-processing (do first, low risk).** ADR-0009 already composites
   client-side; deepen it. WebCodecs `VideoFrame` → WebGPU `importExternalTexture` is a **zero-copy**
   path, so the client runs luma-guided **4:2:0→4:4:4 chroma reconstruction**, deblocking,
   text-sharpening, and dithering on its own GPU — quality that would otherwise cost the host the
   **AVC444 double-encode** (ADR-0009 §3) or a 10-bit path.
2. **Client-side upscaling / super-resolution (high density payoff).** Host encodes the **video lane**
   at reduced resolution under host-GPU or network pressure; the client reconstructs — Lanczos in WebGL
   (baseline) or FSR/ESRGAN/Anime4K-class ML in WebGPU/WebNN. Encoding 720p instead of 1080p is ~0.44×
   the pixels → roughly **halves NVENC GPU-time and bitrate** on that region, turning ADR-0007's "drop
   resolution" rung into a **much less visible** one.
3. **Command / cached-tile UI lane (biggest bandwidth win, but architecturally gated).** The
   RDP-GFX/RemoteFX/Guacamole model — drawing primitives + a glyph/tile cache, client GPU rasterizes
   crisp text at near-zero bandwidth. The **full** command lane needs guest-side draw-op capture we
   don't have (infiniPixel captures a framebuffer, not GDI/DX calls); the **achievable-now half** is
   **content-addressed tile/glyph caching** (dedup identical tiles client-side), needing no guest
   changes.
4. **Client-side frame interpolation / reprojection (narrow, video-lane-only).** Reserve it for the
   **video island**, never the interactive UI — interpolation *adds* latency (it holds a frame) while
   reprojection/extrapolation *hides* it, and the desktop's biggest reprojection win (the local cursor
   sprite) is already in ADR-0009.

All four are **capability-negotiated**; a browser that advertises only baseline WebGL2 + WebCodecs
falls back to the ADR-0009 pixel path unchanged.

---

## 0. Capability negotiation & the offload tiers

The client advertises a capability descriptor at session setup (reliable control stream, extending
ADR-0009's per-session codec negotiation):

| Capability | Probe | Unlocks |
|---|---|---|
| **WebCodecs codecs** | `VideoDecoder.isConfigSupported()` for H.264/HEVC/AV1 | which codec the host encodes (already in ADR-0009) |
| **WebGL2** | context creation | baseline compositing, Lanczos/bicubic upscale, chroma upsample, cursor sprite |
| **WebGPU** | `navigator.gpu.requestAdapter()` | compute shaders → ML super-res, ping-pong post-process, neural interpolation |
| **WebNN / NPU** | `navigator.ml.createContext({deviceType:'npu'})` | offload super-res to a dedicated NPU, sparing the client's own render GPU |
| **Decoder MV export** | (none today) | codec motion vectors for reprojection — **NEEDS VERIFICATION**, see §4 |

The host maps this to an **offload profile** and **degrades to the pixel path per capability** — a
locked-down kiosk browser or an old tablet simply gets ADR-0009 as written. WebGPU is now safe to
*prefer* but not *require*: as of 2026 it ships by default in Chrome/Edge (113+), Firefox (141
Windows, 145 macOS), and Safari 26, ~85% global coverage — but Linux/Android Firefox are still in
progress, so WebGL2 remains the mandatory floor
([web.dev](https://web.dev/blog/webgpu-supported-major-browsers),
[MDN WebGPU](https://developer.mozilla.org/en-US/docs/Web/API/WebGPU_API)).

---

## 1. Command-based UI rendering — the command / cached-tile lane

**The model.** Every serious remote-desktop protocol sends the static UI as *drawing operations and
cached bitmaps*, not pixels. **Apache Guacamole** streams instructions "generally drawing
instructions (caching, clipping, drawing images), using the client as a remote display," and its
manual is explicit that "these primitives … take up less bandwidth than sending corresponding PNG
images" — the client rasterizes them onto an HTML5 canvas
([Guacamole protocol](https://guacamole.apache.org/doc/gug/guacamole-protocol.html)). The concrete
server→client verbs are a Cairo-like 2D API — `rect`, `arc`, `curve`, `line`, `cfill`, `cstroke`,
`copy`, `transfer`, `cursor`, plus `img`-stream image delivery and layer/`transform` management
([Guacamole protocol reference](https://guacamole.apache.org/doc/gug/protocol-reference.html)).
**RDP's Graphics Pipeline (MS-RDPEGFX)** goes further: it runs a runtime classifier that
"differentiates text, synthetic image, natural image, and video content … each encoded with a
dedicated codec," keeps a **glyph cache** and a **bitmap cache** (default raised to 100 MB) so a
repeated glyph or UI element "doesn't have to be transmitted," and does surface-to-surface blits and
cache-import to reuse pixels already on the client
([RemoteFX Adaptive Graphics](https://techcommunity.microsoft.com/blog/microsoft-security-blog/remotefx-adaptive-graphics-in-windows-server-2012-and-windows-8/247454),
[MS-RDPEGFX](https://winprotocoldoc.blob.core.windows.net/productionwindowsarchives/MS-RDPEGFX/%5BMS-RDPEGFX%5D.pdf)).
Text "is the most common content type in Windows, so supporting a codec that is highly efficient for
text is critical."

**How it composes with the ADR-0009 hybrid.** ADR-0009's **UI-tile lane** (reliable, near-lossless
4:4:4 dirty-rect tiles) becomes, for a capable client, a **command lane**: the same reliable stream,
but carrying draw-ops + cache references instead of encoded image tiles. The video lane is untouched.
The client composites command-rasterized UI *under* the decoded video island and *under* the cursor
sprite — exactly the layering ADR-0009 already builds.

**The hard problem, stated honestly.** RDP and Guacamole get drawing commands because they sit *above*
the drawing API — at GDI/DirectX/X11. infiniPixel deliberately sits at the **rendered-framebuffer
seam** (ADR-0005/0009: Windows IddCx display-first, Linux DRM/KMS deliver a *framebuffer*, not a
draw-op stream). So the host has **pixels, not commands**, and cannot emit a true vector command lane
without a **new guest-side 2D-interception layer** (a cooperative-guest roadmap item, sibling to
ADR-0007's damage tracking). Deriving commands from pixels (OCR/vectorization) is lossy and expensive
— rejected.

**The achievable-now half (no guest changes): content-addressed tile/glyph caching.** The half that
needs guest draw-ops is *vector* rendering; the half that needs **nothing new** is *deduplication*:
hash each dirty tile (32×32 / 64×64 block) and, on a cache hit, send a **cache reference (a few
bytes)** instead of the encoded tile. A desktop is saturated with repeats — the same glyph bitmap,
window chrome, icon, scrolled-in row — so a warm client tile cache turns most "changed" tiles into
references. This is RDP's bitmap cache / `RDPGFX_CMDID_CACHEIMPORTOFFER` over ADR-0009's reliable
stream, and it is pure host CPU/hash work — **zero host GPU encode** for cache-hit tiles.

**Quantified savings — a text-heavy desktop.** Take the active-but-static case ADR-0009's idle win
doesn't cover: a user typing/scrolling a document. In the **tile lane**, each changed text line is a
near-lossless screen-content tile; a 1920×1080 text page redrawn as screen-content is realistically
~0.3–1.0 Mbit, and every edit re-sends the changed tiles. In the **command lane** (glyph cache warm),
that page is ~3000 glyphs × ~3 bytes of cache-reference + position ≈ **~9 KB (~70 kbit)** — a
**~5–15× bandwidth cut**, and re-rendering already-cached text is **~0 bits**. The **host-encode
saving is the bigger prize for ADR-0007**: command-rendered or cache-hit UI regions run **no NVENC
pass and no screen-content intra pass at all** — the two GA102 NVENC blocks are freed from *all*
static-UI work and reserved purely for genuine video/3D islands, which is precisely the density
multiplier ADR-0007 wants. Cost/caveat: Guacamole warns "excessive use of primitives leads to an
increase in client-side processing," so the classifier keeps photographic tiles on the codec path and
only vector/cache-refs the UI — the hybrid split already does this.

---

## 2. Client-side upscaling / super-resolution

**The lever.** Under host-GPU contention (ADR-0007) or network pressure, the host encodes the **video
lane** at *reduced* resolution and the client reconstructs to native. This is exactly how GeForce NOW
already ships: "your local device receives and decodes a compressed video stream at a … Streaming
Resolution [that may not] match … Display Resolution," and GFN "resolution scaling is processed and
applied on your local device," including an **AI-enhanced mode** that "leverages the … GPU to pass
content through a trained neural network model"
([NVIDIA GFN scaling](https://nvidia.custhelp.com/app/answers/detail/a_id/5250/~/how-does-geforce-now-resolution-scaling-work)).
NVIDIA frames the next phase as **hybrid compute**: "the server renders … transmits a compressed
stream, and the local device's … NPU runs a super-resolution model to upscale the final output"
([NVIDIA AI upscaling](https://blogs.nvidia.com/blog/ai-decoded-upscaling/)).

**In-browser feasibility (real, 2026).** Two tiers:
- **Spatial (baseline, WebGL2):** Lanczos/bicubic in a fragment shader, sub-millisecond, universally
  supported, zero hallucination risk. A direct WebGL port of **AMD FSR 1.0** exists
  ([web-fsr](https://github.com/Hajime-san/web-fsr)); AMD's own 2026 "Upscale Everything" push spans
  its hardware ([AMD](https://www.amd.com/en/developer/resources/technical-articles/2026/upscale-everything-super-resolution-across-amd-hardware-.html)).
- **ML super-res (WebGPU/WebNN):** production-proven in the browser today. **WebSR** ports Anime4K and
  Real-ESRGAN to WebGPU compute for "real-time super resolution to videos on the web"
  ([WebSR](https://github.com/sb2702/websr)); **Anime4K-WebGPU** upscales/denoises/deblurs entirely
  client-side ([Anime4K-WebGPU](https://github.com/Anime4KWebBoost/Anime4K-WebGPU)); the **Free AI
  Video Upscaler** hit 250k MAU on a **WebGPU + WebCodecs, zero-server** stack
  ([web.dev](https://web.dev/case-studies/ai-video-upscaler-case-study)). WebSR notes super-res is
  "relatively better … [on] … Screen-sharing content" — VDI-shaped content. **WebNN** reached W3C
  Candidate Recommendation (Jan 2026), lists **super resolution** as a use case, and is "the only web
  API that enables NPU access," so an NPU client runs super-res *without* touching its render GPU
  ([WebNN](https://www.w3.org/TR/webnn/),
  [Microsoft](https://learn.microsoft.com/en-us/windows/ai/directml/webnn-overview)).

**Quality vs latency, and the routing rule.** Spatial upscale is cheap and safe but soft; ML super-res
is sharper on *natural* images but can **hallucinate on text** — fatal for the legibility constraint.
So the routing follows the hybrid: **never ML-upscale the UI lane** (it stays crisp via §1's command/
tile lane); apply upscaling **only to the video/3D island**, where ML models are trained and where
softness is acceptable. Budget: Anime4K-class WebGPU passes run in the **~3–4 ms/frame** range on a
modest client GPU (consistent with the RIFE runtimes in §4) — inside a 60 fps frame budget.

**Host signalling.** The control stream carries a per-region descriptor
`{region_id, native_w, native_h, sent_w, sent_h, scaler_hint}`; the client decodes at `sent_*` and
upscales the video texture to `native_*` during composition (§3), keying the algorithm off
`scaler_hint` and its own capability tier.

**Host-GPU + bandwidth saving.** NVENC GPU-time and bitrate scale roughly with pixel count, so sending
**720p for a 1080p region ≈ 0.44× pixels ≈ ~halved encode time and bitrate** on that island; 960×540
for 1080p is ~0.25×. This is a direct ADR-0007 density lever *and* it makes ADR-0009's degradation
ladder ("spend fps → bitrate → **resolution** → foveation") far less visible — the resolution rung
now degrades to *client-reconstructed* native rather than a visibly blurry stretch. **NEEDS
VERIFICATION:** measured VMAF/SSIMULACRA2 of client-reconstructed 720p→1080p vs native-encoded 1080p
at equal wire bitrate, on real VDI video content.

---

## 3. Client-side composition + post-processing

ADR-0009 already composites three layers client-side (UI tiles + cropped video + cursor sprite). The
offload insight is to **move host-side pixel-quality work into that same client compositor** — the
modern browser GPU stack makes it nearly free.

**Zero-copy plumbing.** WebGPU added `importExternalTexture` for a WebCodecs `VideoFrame`, "a
zero-copy operation, using the exact same VideoFrame object in memory within a WebGPU pipeline … the
most performant method for rendering a VideoFrame"
([Chrome WebGPU](https://developer.chrome.com/blog/new-in-webgpu-116/),
[webgpufundamentals](https://webgpufundamentals.org/webgpu/lessons/webgpu-textures-external-video.html)).
A 2026 browser video compositor demonstrates the exact shape we need: "video textures as
texture_external (zero-copy), with compositing running through a ping-pong WGSL shader pipeline"
([HN](https://news.ycombinator.com/item?id=46959456)). So the decoded video never leaves the client
GPU between decode and screen.

**Post-process passes the client GPU runs, and what each saves the host:**

| Client pass | What it does | Host work it removes / avoids |
|---|---|---|
| **Luma-guided 4:2:0→4:4:4 chroma reconstruction** | upsample U/V using the full-res Y channel (+ a cheap host-sent text-edge hint map) — the "AI model leverages the full-resolution Y channel to enhance the U and V channels" pattern ([Fluendo](https://fluendo.com/blog/synthetic-data-generator-for-ai-chroma-upsampling/)) | avoids ADR-0009's **AVC444 double-encode** on text tiles (which "roughly doubles the pixel work of that region", ADR-0009 §3) — the host sends plain 4:2:0 + a small edge hint |
| **Deblocking / deringing** | smooth block/ring artifacts on low-bitrate video islands | lets the host encode the island at a **lower bitrate** (ADR-0007 budget) without the artifacts showing |
| **Text sharpening** | luma unsharp-mask on classifier-flagged text edges | recovers legibility the host would otherwise buy with 4:4:4 / lower QP bits |
| **Dithering** | hide 8-bit banding on flat gradients | avoids a host **10-bit encode** path for banding-prone UI |

Honest framing: some of these (deblock, sharpen, dither) are *quality the host never did* —
infiniPixel's host path is fixed-function NVENC — so the offload value is that **quality which would
otherwise cost host bits or a heavier encode is instead synthesized on the client GPU for free**. The
chroma-reconstruction pass is the cleanest *direct* host-encode saving: it can retire the AVC444
special case if reconstruction clears the ADR-0009 edge-fidelity gate. **NEEDS VERIFICATION:** whether
luma-guided 4:2:0→4:4:4 + edge hints clears that hard text-legibility gate on real UI fonts.

---

## 4. Client-side frame interpolation / reprojection

**Three techniques — and the latency sign matters.**
- **Reprojection / timewarp (ATW analog):** warp the *last* received frame to a newer state just
  before present. VR's ATW "reprojects an already rendered frame just before sending it to the headset
  … [to] reduce perceived latency"; ASW "extrapolates a new frame … using depth information"
  ([Asynchronous reprojection, Wikipedia](https://en.wikipedia.org/wiki/Asynchronous_reprojection)).
  Reprojection/extrapolation **hides** latency.
- **Interpolation (DLSS-FG / RIFE analog):** synthesize an in-between frame from *two* received frames.
  It **adds** latency because it must **hold the newest frame** — "it inherently adds input latency due
  to the fact that it holds a frame," ~10 ms on DLSS 3, why DLSS-FG mandates Reflex
  ([TechSpot DLSS 4](https://www.techspot.com/article/2945-nvidia-dlss-4/)). It improves *smoothness*,
  not responsiveness.

**In-browser feasibility (real).** Neural interpolation runs in the browser today: **RIFE v4.9**
exported to ONNX and run via **ONNX Runtime Web on the WebGPU backend**, "2×–6× interpolation at
**3–4 ms per frame**" ([RIFE-class WebGPU runtime, per 2026 web-dev reports]). The 2026 **ANVIL**
work is directly on-point for us — it reuses **codec motion-vector priors** (block-level H.264 MVs
"extracted during decoding, converted to a dense flow field … used to warp both input frames") to
make interpolation cheap ([ANVIL, arXiv 2603.26835](https://arxiv.org/html/2603.26835v1)). The catch:
**WebCodecs does not currently expose decoder motion vectors**, so a browser client must either
compute optical flow itself (costlier) or the **host must send MV/flow hints** on a side channel —
**NEEDS VERIFICATION** on both feasibility and value.

**The desktop-specific position.** For a *2D interactive desktop*, blanket interpolation is wrong: UI
has hard edges, text, and content that *pops into existence* (menus, tooltips), which interpolation
smears — high artifact risk — and it adds latency to the one thing that must stay responsive. Three
disciplined uses instead:
1. **Cursor reprojection (already in ADR-0009):** the local HW-cursor sprite is the desktop's biggest
   reprojection win — the pointer moves at input latency, never re-encoding a frame. Nothing to add.
2. **Scroll reprojection (cheap, guest-cooperative):** a scroll has a *known* vector. If the guest
   reports the scroll delta (a cooperative-guest hint, sibling to ADR-0007 damage), the client can
   shift the last frame by that vector to synthesize a smooth intermediate frame at a *lower* host
   frame rate — filling the newly-revealed edge from the tile cache (§1). Low artifact risk because
   the motion is exactly known, not estimated.
3. **Video-island interpolation (opt-in, latency-tolerant):** for a *video being watched* (not
   interacted with frame-precisely), the host sends that region at e.g. **30 fps** and the client
   interpolates to 60 for smoothness. This **halves host encode + bandwidth on the island**, and the
   +1-frame latency it adds is invisible on passive video (you don't click frame-accurately inside a
   movie). The interactive UI path stays strictly 1-in-1-out. This is the RIFE/DLSS-FG value applied
   exactly where its latency cost is free.

**Extrapolation** (predict forward, no held frame — Intel's "AI-generated and no input latency" goal;
[TweakTown](https://www.tweaktown.com/news/102083/intel-is-working-on-the-holy-grail-of-frame-generation-ai-generated-and-no-input-latency/index.html))
is the ideal desktop tool in theory (smoothness *and* latency-hiding) but riskiest for disocclusion/
ghosting — filed as a research option, **NEEDS VERIFICATION** for production 2D-desktop quality.

---

## 5. Integration — offload map, host changes, savings

| Offload (client GPU) | Host-side change | Client requirement | Host-GPU saving | Bandwidth saving | Latency effect | ADR tie |
|---|---|---|---|---|---|---|
| **Chroma reconstruction + post-process** (§3) | send 4:2:0 + edge-hint map; drop AVC444 | WebGL2 (WebGPU better) | retires AVC444 double-encode on text | — | neutral | 0009 §3 legibility |
| **Reduced-res video + client super-res** (§2) | encode island at `sent_*`; send res descriptor | WebGL2 (Lanczos) / WebGPU / NPU | **~2–4× less encode** on island | **~2–4× less** on island | +~3–4 ms client | 0007 ladder, 0009 res rung |
| **Command / cached-tile UI lane** (§1) | tile-hash + cache-ref (now); guest draw-op capture (later) | WebGL2 2D raster + tile cache | **no NVENC/SCC pass** on static UI | **~5–15× less** on active text | neutral | 0009 UI lane, 0007 density |
| **Video-island interpolation** (§4) | send island at ½ fps; optional MV hints | WebGPU (neural) / WebGL (warp) | **~½ encode** on island | **~½** on island | **+1 frame on island only** | 0009 fps rung |

**Fallback ladder (per capability).** infiniPixel-offload (WebGPU + NPU) → infiniPixel-lite (WebGL2:
composite + Lanczos + cache-refs) → **ADR-0009 pixel path unchanged** (WebCodecs decode + WebGL
composite, host does all encode work) → SPICE (thin/legacy). The host always keeps the ADR-0009 path
correct; offload is strictly additive.

**Risks / open problems (NEEDS VERIFICATION).** (a) The command lane's full win needs a guest 2D
draw-op interception layer that does not exist — only the cache-dedup half is free today. (b)
ML super-res and interpolation must be *proven not to violate the text-legibility gate* — hence the
routing rule that keeps them off the UI lane. (c) Client GPU budget is now a scheduling variable: a
weak client may not afford super-res + interpolation + composite at 60 fps, so the host must adapt
offload depth to *client* capacity as well as its own — a new dimension in ADR-0009's adaptive control
loop. (d) WebCodecs MV export is absent, gating cheap interpolation on a host-sent hint channel. (e)
Splitting work across two GPUs adds a synchronization/versioning contract (which frame a cache-ref or
res-descriptor applies to) that the wire protocol (ADR-0004/doc 11) must carry.

## Recommendation (in one line)

**Add a capability-negotiated client-offload layer to infiniPixel that (1) composites and
post-processes on the client GPU via zero-copy WebCodecs→WebGPU — reconstructing 4:4:4 from
luma-guided 4:2:0 to retire AVC444; (2) encodes the video island at reduced resolution and lets the
client super-resolve it (Lanczos/WebGL baseline, FSR/ESRGAN/Anime4K WebGPU or WebNN-NPU when
advertised) — ~halving NVENC time and bitrate per ADR-0007; (3) turns ADR-0009's UI-tile lane into a
content-addressed cached-tile lane now (no guest changes, ~5–15× less text bandwidth and zero encode
on static UI) with a full draw-op command lane as a cooperative-guest roadmap item; and (4) applies
frame interpolation only to the passive video island (host sends ½ fps, client interpolates) and never
to the interactive path — with a mandatory thin-client fallback to the unchanged ADR-0009 pixel path.**

## Sources

- Apache Guacamole — protocol (client-side drawing instructions, "less bandwidth than … PNG"): https://guacamole.apache.org/doc/gug/guacamole-protocol.html
- Apache Guacamole — protocol reference (rect/arc/curve/line/cfill/cstroke/copy/transfer/cursor/img/cache verbs): https://guacamole.apache.org/doc/gug/protocol-reference.html
- Microsoft — RemoteFX Adaptive Graphics (content differentiation, glyph/bitmap cache, 100 MB cache): https://techcommunity.microsoft.com/blog/microsoft-security-blog/remotefx-adaptive-graphics-in-windows-server-2012-and-windows-8/247454
- MS-RDPEGFX — Graphics Pipeline Extension (surface ops, cache import, glyph/text): https://winprotocoldoc.blob.core.windows.net/productionwindowsarchives/MS-RDPEGFX/%5BMS-RDPEGFX%5D.pdf
- FreeRDP — Codecs (RemoteFX Progressive, ClearCodec, AVC420/AVC444, GFX hybrid): https://freerdp-freerdp.mintlify.app/concepts/codecs
- GeForce NOW — how resolution scaling works (client-device upscale, AI-enhanced NN mode): https://nvidia.custhelp.com/app/answers/detail/a_id/5250/~/how-does-geforce-now-resolution-scaling-work
- NVIDIA — AI-powered upscaling (server renders, client/NPU super-resolves — hybrid compute): https://blogs.nvidia.com/blog/ai-decoded-upscaling/
- WebSR — real-time browser super-resolution (Anime4K/Real-ESRGAN in WebGPU; screen-sharing content): https://github.com/sb2702/websr
- Anime4K-WebGPU — client-side upscale/denoise/deblur via WebGPU compute: https://github.com/Anime4KWebBoost/Anime4K-WebGPU
- web.dev — Free AI Video Upscaler (WebGPU + WebCodecs, zero-server, 250k MAU): https://web.dev/case-studies/ai-video-upscaler-case-study
- web-fsr — AMD FSR 1.0 ported to WebGL: https://github.com/Hajime-san/web-fsr
- AMD — Upscale Everything: Super-Resolution Across AMD Hardware (2026): https://www.amd.com/en/developer/resources/technical-articles/2026/upscale-everything-super-resolution-across-amd-hardware-.html
- W3C — Web Neural Network (WebNN) API (CR; super-resolution use case; NPU): https://www.w3.org/TR/webnn/
- Microsoft — WebNN overview (GPU/CPU/NPU acceleration, DirectML): https://learn.microsoft.com/en-us/windows/ai/directml/webnn-overview
- Fluendo — AI chroma upsampling (full-res Y guides U/V enhancement): https://fluendo.com/blog/synthetic-data-generator-for-ai-chroma-upsampling/
- Chrome — What's New in WebGPU 116 (VideoFrame → GPUExternalTexture, copyExternalImageToTexture): https://developer.chrome.com/blog/new-in-webgpu-116/
- webgpufundamentals — Using video efficiently (texture_external, textureSampleBaseClampToEdge): https://webgpufundamentals.org/webgpu/lessons/webgpu-textures-external-video.html
- Hacker News — browser-based WebGPU video compositor (zero-copy texture_external, ping-pong WGSL): https://news.ycombinator.com/item?id=46959456
- Asynchronous reprojection — Wikipedia (ATW rotational warp; ASW depth/MV extrapolation, intermediate frames): https://en.wikipedia.org/wiki/Asynchronous_reprojection
- Meta Horizon — Asynchronous Spacewarp (motion vectors from GPU video-encoder, frame extrapolation): https://developers.meta.com/horizon/blog/asynchronous-spacewarp/
- TechSpot — DLSS 4 review (interpolation holds a frame → adds latency; Reflex compensation): https://www.techspot.com/article/2945-nvidia-dlss-4/
- TweakTown — Intel frame extrapolation ("AI-generated and no input latency"): https://www.tweaktown.com/news/102083/intel-is-working-on-the-holy-grail-of-frame-generation-ai-generated-and-no-input-latency/index.html
- ANVIL — Accelerator-Native Video Interpolation via Codec Motion-Vector Priors (arXiv 2603.26835): https://arxiv.org/html/2603.26835v1
- RIFE — Real-Time Intermediate Flow Estimation (ECCV 2022; ONNX/WebGPU runtimes): https://github.com/hzwer/ECCV2022-RIFE
- web.dev — WebGPU now supported in major browsers (Chrome/Edge/Firefox/Safari 26): https://web.dev/blog/webgpu-supported-major-browsers
- MDN — WebGPU API (browser support; compute shaders vs WebGL): https://developer.mozilla.org/en-US/docs/Web/API/WebGPU_API
- Prior corpus: ADR-0007 (VDI capacity manager / density), ADR-0009 (infiniPixel protocol), docs 09/16/17/18/22/23
