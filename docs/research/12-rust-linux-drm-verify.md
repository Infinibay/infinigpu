# 12 — Verifying "Can the Linux guest DRM/KMS driver be Rust?" (2026)

**Task:** Reconcile the doc-04 vs doc-05 disagreement and get the precise, current
(July 2026) truth on whether a **Linux guest DRM/KMS *display* driver** can be written in
Rust today. Verified against the live Linux DRM tree (master = **7.2.0-rc3**) and the
dri-devel/nouveau mailing lists.

**Verdict: REFUTED (as stated).** A KMS/atomic-modeset *display* driver **cannot** be
written in pure **upstream** Rust today: at Linux 7.2-rc3 the mainline tree ships Rust
abstractions for the DRM *render/buffer* path only — there is **no upstream Rust KMS
abstraction at all**. Doc 04 was right; doc 05 over-rated the guest driver as "Rust
Medium-High" by conflating render-node maturity with KMS maturity, and its VGEM-in-Rust
proof point does not apply to display. Details and the corrected recommendation below.

---

## 1. What Rust DRM abstractions are actually upstream (7.2-rc3)

I read the mainline tree directly. `rust/kernel/drm/mod.rs` on `torvalds/linux` master
declares exactly these submodules (verbatim `pub mod`):

- `device` — `drm::Device`
- `driver` — `drm::Driver` (the `Driver` trait: `DriverInfo`, feature flags, IOCTL table)
- `file` — `drm::File`
- `gem` — GEM object abstraction (+ the **GEM shmem helper**, landed in **7.1**)
- `gpuvm` — GPU virtual-memory (the **immediate-mode** abstraction landed in **7.2**)
- `ioctl` — DRM ioctl definitions
- (`private`)

The in-tree rendered docs (`rust.docs.kernel.org/next/kernel/drm`) corroborate this:
device, driver, file, gem, ioctl (gpuvm is config-gated in that docs build). Linux 7.1's
DRM-Rust merge added the DMA-coherent API rework, a **GPU buddy allocator** abstraction,
the **DRM shmem GEM helper**, and I/O/workqueue plumbing — all still render/buffer-side.

**What is NOT in the mainline `drm` Rust tree at 7.2-rc3:**

- **No `kms`, `crtc`, `plane`, `connector`, `encoder`, `atomic`, or `modeset` module.**
- **No `sched` (GPU scheduler), `syncobj`, or `dma_fence` module.** These appeared in
  Asahi Lina's 2023 RFC (`drv/device/file/gem/mm/ioctl/gpu-scheduler/dma_fence/syncobj`)
  but were **not** carried into mainline as `drm` submodules — the scheduler/fence
  pieces still live out-of-tree / in the Nova & Tyr enablement series, not as stable
  upstream abstractions. (NEEDS VERIFICATION only on whether a *non-drm* `dma_fence`
  helper landed elsewhere; it is not exposed under `kernel::drm`.)

So the upstream Rust surface is the **render node**: register a `drm::Driver`, expose
GEM buffers, define ioctls, manage a GPU address space (gpuvm). That is precisely the
half of a DRM driver a **display-only** guest driver does *not* need — and it is missing
exactly the half (KMS) that a display driver *is*.

## 2. KMS/atomic-modeset in Rust: only an out-of-tree WIP RFC

The claim to test is specifically about **KMS**. There is exactly one Rust-KMS effort,
and it is **not upstream**:

- **"Rust bindings for KMS + RVKMS"** by **Lyude Paul (Red Hat)**. It introduces driver
  traits `DriverCrtc`, `DriverPlane`, `DriverConnector`, `DriverEncoder`, typed
  containers `Crtc<T>`/`Plane<T>`/…, and atomic-state traits `CrtcState`/`PlaneState`/
  `ConnectorState`, using a **typestate pattern** to make init-ordering errors a
  compile error. **RVKMS** is a Rust port of the virtual **VKMS** driver used as the
  first consumer.
- **Status: WIP RFC, unmerged.** The latest public revision I could confirm is **WIP RFC
  v2 (35 patches), Dec 2024** (e.g. "`WIP: rust: drm/kms: Add drm_crtc bindings`", patch
  07/35, 12 Dec 2024). The LWN write-up (20 Nov 2024) states the bindings are
  "not yet upstream," "early-stage," capable only of **basic KMS registration + VBLANK
  emulation**, and that most effort has gone into the bindings rather than RVKMS itself.
- **Still true in 2026.** As of **Jan 2026**, Lyude Paul was still posting prerequisite
  refactors — **`DeviceContext` v5** — that fix the multi-step DRM device-init lifecycle
  in Rust (a blocker *before* KMS registration can be made sound). KMS bindings had not
  merged; the 7.2-rc3 tree confirms no KMS module exists.

The earlier "add driver abstractions" series (v2, Sep 2024) that *did* aim for mainline
**explicitly excluded KMS** — Danilo Krummrich's design note describes KMS as a *future*
"another associated `Kms` type for `Driver`," not part of the merged work.

**Conclusion (2):** KMS/atomic-modeset Rust bindings are **out-of-tree WIP only**
(Lyude Paul's series + Asahi's driver-private bindings). A display driver written in
"pure upstream Rust" is **not possible at 7.2-rc3.** Doc 04's claim holds; doc 04's
citation to the RVKMS RFC was correct.

## 3. VGEM-in-Rust does NOT prove KMS-in-Rust — confirmed

Doc 05 leaned on "the VGEM virtual DRM driver was rewritten in Rust … proof that a whole
small DRM driver can be Rust." Checked against Maíra Canal's (Igalia) write-up and the RFC:

- **VGEM is a render/dma-buf *test* driver with no display pipeline.** The Rust VGEM
  (~500 LOC) implements a platform device + **two ioctls** — `drm_vgem_fence_attach` and
  `drm_vgem_fence_signal` — over `dma_resv`/dma-fence and GEM. It has **zero KMS**: no
  CRTC, plane, connector, encoder, atomic commit, or scanout. It puts no pixels anywhere.
- It was still an **RFC/PR** (rebasing onto the new pin-init API), not a clean mainline
  merge, at the time of the sources.

So VGEM-in-Rust proves the **render-node** abstractions (device/driver/gem/ioctl/fence)
are usable in Rust — which is real and useful — but it says **nothing** about a display
driver. Using it as evidence that a guest *DRM/KMS* driver can be Rust is a category
error. **doc 05's proof point is refuted for the display case.**

## 4. Nova / nova-drm: wrong side of the VM, and no KMS yet — confirmed

- **Nova is a *host* NVIDIA driver for real GSP hardware** (`nova-core` in mainline since
  ~6.15; `nova-drm` under `drivers/gpu/drm/nova/`). It boots the GSP, runs command
  queues, and exposes DRM userspace interfaces for *physical* Turing+ GPUs (incl. GA102).
- **nova-drm does not implement KMS/display yet.** Per the nouveau list and the RVKMS
  discussion, display is a *future* phase: the plan is to *first* land the Rust KMS
  bindings (via RVKMS) and *then* write nova's modeset driver. Nova's mainline
  submissions (6.15+) contained **no display/KMS**.
- **It does not help a guest paravirtual driver.** A guest paravirtual display driver
  talks a private command ring to a host device model; Nova talks GSP firmware to silicon.
  The only overlap is the shared *render-node* Rust abstractions Nova exercises — which,
  again, are not the KMS layer a guest *display* driver needs. Nova is at best a distant
  proving-ground; at worst a red herring for this decision.

## 5. Reconciling doc 04 vs doc 05

| Point | doc 04 | doc 05 | Verified truth (7.2-rc3) |
|---|---|---|---|
| KMS/modeset Rust upstream? | **No** (out-of-tree/RFC only) | implied "landing/viable" | **doc 04 correct** — no KMS module in mainline |
| Render-node Rust upstream? | "closer" | Yes (gem/ioctl/gpuvm) | **both roughly right** — device/driver/file/gem/gpuvm/ioctl are upstream |
| VGEM-in-Rust ⇒ display driver can be Rust? | (not claimed) | **claimed** | **doc 05 wrong** — VGEM has no KMS |
| Guest DRM leaf driver language | **C for M1** | **Rust Medium-High** | **doc 04 correct for the display milestone** |

doc 05 is not wrong that a *render*-node leaf driver can be substantially Rust; it is
wrong to extend that to the **guest display driver**, whose entire job is KMS. The
"Rust Medium-High" rating and "the shared crate drops straight in" optimism apply to the
render path, not to milestone-1 pixels-on-screen.

## 6. Recommendation for infinigpu's Linux guest paravirtual DRM/KMS driver

**Write the KMS/modeset layer in C; keep the wire protocol as a Rust `no_std` crate that
is the *source of truth* for the ABI but manifests to the C kernel module as a generated
C header.** Rationale and the exact split:

**Kernel-version target.** The C DRM/KMS UAPI and helpers (`drm_simple_display_pipe`,
`drm_gem_shmem`, dumb buffers, atomic helpers) are **stable and present on every current
kernel** — no special version needed; target whatever stable the guests ship (7.x and
comfortably older). *If and when* a render/3D node is added, target **≥ 7.2** where the
Rust `gem`/`gem-shmem`/`gpuvm`/`ioctl`/`file`/`driver` abstractions are upstream — but
even then **KMS stays C** until Lyude Paul's bindings merge.

**Layer-by-layer language:**

| Layer | Language | Why |
|---|---|---|
| Guest **KMS/modeset** (crtc/plane/encoder/connector, atomic_check/commit, dumb FBs, pageflip/vblank) | **C** | No upstream Rust KMS at 7.2-rc3; only a WIP RFC. This is ~all of milestone 1. |
| Guest **GEM / buffer mgmt** | **C** (M1) | shmem-GEM Rust exists (7.1) but a mixed Rust-KMS/C-GEM module is not a thing; keep M1 uniformly C. |
| **Command-ring / wire protocol / serialization / handle lifecycle** | **Rust `no_std`** (ABI source-of-truth) | pure data-structure logic, property-testable, **shared with the host backend and the Windows driver**. For the guest C KMD it manifests as a **cbindgen-generated C header** (structs/constants), not linked Rust object code. |
| Guest **render/3D submission ioctl** (milestone 2) | **Rust on ≥7.2 possible** | gem/ioctl/file/gpuvm are upstream — but this is *not needed for the display milestone*. |
| Guest **Mesa UMD** | **C** | no pure-Rust Mesa driver. |

**Do NOT** gate milestone 1 on a pure-Rust driver carrying out-of-tree WIP KMS bindings:
they are unmerged, single-maintainer, tied to the nova/RVKMS cadence, and a permanent
rebase treadmill against a fast-moving in-tree API. Reassess only once KMS bindings land
upstream. (Dave Airlie's oft-quoted "DRM is ~a year from *requiring* Rust for new
drivers" is a 2026-forward trajectory, **NEEDS VERIFICATION** as a current mandate — it
is not today's reality, and it concerns *new upstream* drivers, not an out-of-tree
paravirtual guest module we ship ourselves.)

**Honest nuance on "shared Rust crate drops into the guest kernel."** RfL modules are
Rust modules *or* C modules; Kbuild does not cleanly link an arbitrary Rust staticlib into
a C kernel module. So the shared Rust ring crate realistically serves the **host backend,
the Windows guest driver, and guest userspace**; the **guest C KMD mirrors the same wire
ABI via a generated header**. The format is small and fixed, so a `cbindgen` header plus a
round-trip conformance test keeps both sides byte-identical — the same discipline
infinibay already uses for the HMAC/virtio-serial contract. This is stronger than doc
05's "drops straight in," and it costs nothing at milestone 1.

**Net:** the language question resolves cleanly. The layer where infinigpu's guest driver
lives (KMS) is exactly the layer upstream Rust does **not** cover; the layers Rust covers
(render node) are exactly the ones the display milestone doesn't touch. Ship **C for the
guest KMS/KMD**, **Rust for the protocol/ring crate + the whole host backend**, and revisit
a Rust render node on ≥7.2 later.

---

## Sources

- Linux master `rust/kernel/drm/mod.rs` (submodule list: device/driver/file/gem/gpuvm/ioctl): https://github.com/torvalds/linux/tree/master/rust/kernel/drm and https://raw.githubusercontent.com/torvalds/linux/master/rust/kernel/drm/mod.rs
- In-tree rendered Rust docs, `kernel::drm` (device/driver/file/gem/ioctl): https://rust.docs.kernel.org/next/kernel/drm/index.html
- Linux master `Makefile` (VERSION 7 / PATCHLEVEL 2 / SUBLEVEL 0 / -rc3): https://raw.githubusercontent.com/torvalds/linux/master/Makefile
- Phoronix — "A Lot Of Rust Graphics Driver Changes For Linux 7.1" (shmem GEM helper, buddy allocator, DMA-coherent rework): https://www.phoronix.com/news/Rust-DRM-For-Linux-7.1
- Phoronix — "Nova Continues Being Built Up In Linux 7.2 … DRM Rust" (7.2: GPUVM immediate-mode, HRT): https://www.phoronix.com/news/Linux-7.2-DRM-Rust
- dri-devel — "rust: drm: add driver abstractions" v2 (Sep 2024; excludes KMS; future `Kms` type note): https://www.mail-archive.com/dri-devel@lists.freedesktop.org/msg507868.html
- dri-devel — "[RFC WIP 0/4] Rust bindings for KMS + RVKMS": https://www.mail-archive.com/dri-devel@lists.freedesktop.org/msg486701.html
- dri-devel — "[WIP RFC v2 07/35] rust: drm/kms: Add drm_crtc bindings" (12 Dec 2024, still WIP): https://www.mail-archive.com/dri-devel@lists.freedesktop.org/msg521486.html
- LWN 997850 — "RVKMS and Rust KMS bindings" (Lyude Paul, XDC 2024; not upstream; crtc/plane/connector/encoder + atomic; typestate): https://lwn.net/Articles/997850/
- Phoronix — "Rust Bindings Posted For KMS Drivers, VKMS Ported To Rust": https://www.phoronix.com/news/Linux-Rust-KMS-RVKMS
- nouveau list — "[PATCH v5 0/4] Introduce DeviceContext" (Jan 2026; KMS-prerequisite device-init refactor still in flight): https://www.mail-archive.com/nouveau@lists.freedesktop.org/msg51556.html
- Maíra Canal (Igalia) — "Rust for VGEM" (fence_attach/fence_signal ioctls; no KMS; ~500 LOC; RFC): https://mairacanal.github.io/rust-for-vgem/
- Phoronix — "Linux VGEM Driver Rewritten In Rust Sent Out For Review": https://www.phoronix.com/news/Linux-Rust-VGEM-Rewrite-RFC
- LWN 925500 — Asahi Lina "Rust DRM subsystem abstractions" RFC (2023; drv/device/file/gem/mm/ioctl/scheduler/dma_fence/syncobj): https://lwn.net/Articles/925500/
- Rust for Linux — Nova GPU driver (host NVIDIA GSP driver; nova-core/nova-drm): https://rust-for-linux.com/nova-gpu-driver
- Kernel docs — Nova (nova-core is base for VFIO/vGPU/nova-drm; no KMS listed): https://docs.kernel.org/gpu/nova/index.html
- nouveau list — "Future of nouveau/nova's display driver, and rvkms introduction!" (KMS/display is a later phase; RVKMS first): https://www.mail-archive.com/nouveau@lists.freedesktop.org/msg43204.html
- Linux kernel — KMS object model (crtc/plane/encoder/connector, atomic): https://docs.kernel.org/gpu/drm-kms.html
