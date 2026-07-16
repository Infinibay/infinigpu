# 05 — Rust Driver Ecosystem for a From-Scratch GPU-Virtualization Stack (2026)

**Scope:** Can we build the guest drivers (Windows + Linux) and the host backend of `infinigpu`
in Rust in 2026, and where are we forced back into C? This is an honest, cited maturity read for
an architecture decision, not advocacy. Target hardware framing: 2× NVIDIA RTX A5000 (Ampere
GA102) on a Linux KVM/QEMU host, guests running Windows and Linux desktops, time-sliced.

**Bottom line up front:** Rust is now a *first-class, stable-toolchain* kernel language on Linux
and a *real but early* one on Windows. The **host backend and the shared guest user/protocol layers
can be ~pure Rust today**. The **thin OS-specific kernel glue is where Rust is either mature (Linux)
or workable-but-rough (Windows)**, and a few pieces (NVIDIA GSP firmware, the Windows WDDM/DDI
contract, the C DRM/KMS core) are **immovably C or closed** and we consume them across an FFI/ABI
boundary — we do not rewrite them.

---

## 1. Rust-for-Linux: upstream, stable, and what's usable

### Status is no longer "experimental"
Linux **7.0 shipped 12 April 2026** and ended the Rust experiment: the kernel now builds against
the **stable Rust release track only** (nightly no longer required for Rust-enabled builds), with a
**minimum Rust toolchain version** enforced — reported as **Rust 1.93** for 7.0, tracking Debian
stable's toolchain via the project's "Debian anchor" version policy.[1][2][3][4] The Maintainers
Summit decision was explicit that Rust is permanent.[5] DRM maintainer Dave Airlie has said the
DRM subsystem is "about a year away" from *requiring* Rust for new drivers.[6]

For us this removes a real risk that existed a year ago: we can pin a **released stable rustc**, not
a nightly, for the Linux guest kernel module and the host build.

### DRM/GPU Rust abstractions that actually exist in-tree
The Rust GPU story started as Asahi Lina's 2023 RFC of DRM subsystem abstractions —
`drv`/`device`, `file`, `gem`, `mm` (range allocator), `ioctl`, plus `gpu scheduler`, `dma_fence`,
and `syncobj`.[7][8] These have been steadily upstreamed (Danilo Krummrich now drives the DRM-Rust
merges via `drm-rust-next`); the **7.2** cycle adds more abstractions, a **GPUVM immediate-mode**
abstraction, and Higher-Ranked-Lifetime-Type support for device drivers.[6] A `kernel::drm` module
is documented in the in-tree Rust docs, and the **VGEM virtual DRM driver was rewritten in Rust**
(Igalia) as a proof that a whole small DRM driver can be Rust.[6] This last point matters: a
*virtual* GPU device — which is essentially what our guest Linux kernel shim is — is squarely the
kind of leaf driver the Rust abstractions target.

### Two Rust GPU drivers to study
- **Nova** (NVIDIA GSP GPUs): the intended Nouveau successor. Split into **`nova-core`**
  (PCI bring-up, boots the GSP, talks to it over a command queue) and **`nova-drm`** (the DRM
  userspace interfaces). Targets **RTX 20 (Turing) and newer — which includes our Ampere GA102 /
  A5000**. `nova-core` is already in mainline; Hopper/Blackwell FSP boot paths are landing now.[9][10][11]
  Critically, the project explicitly says `nova-core` is designed so **virtualization drivers can be
  built on top of it** — directly relevant if we ever want a host-side Rust component that drives a
  real NVIDIA GPU through the open stack rather than CUDA/Vulkan userspace.
- **Tyr** (Arm Mali) and the **Asahi AGX** driver: additional real-world Rust DRM drivers proving the
  abstractions across vendors.[6][7]

### `no_std` / `alloc` constraints (the real cost of kernel Rust)
Kernel Rust is `#![no_std]`: you get `core` + a kernel-flavored `alloc` + the in-tree `kernel`
crate, **not** `std`. Allocations are **fallible** and GFP-flag-aware; you use `pr_info!`/`pr_err!`,
not `println!`; no threads/files/sockets from `std`, no panics-as-unwind (kernel panics abort).[12][13][14]
Practically: our shared protocol/ring code (see §3) must be written `no_std`-clean and allocator-
agnostic from day one if we want to reuse it inside the Linux module. That's a discipline cost, not
a blocker — it's the same constraint embedded Rust lives under.

### What *must* stay C on Linux
- The **DRM/KMS core, PCI core, scheduler, mm, VFIO core** — Rust drivers are *leaf* drivers that
  call **into C infrastructure through the Rust abstractions**; the cores themselves are C.[6][12]
- **Kbuild/Kconfig** — the build system is Make/C; Rust is bolted in, not replacing it.
- **NVIDIA GSP firmware** is a signed proprietary blob loaded by the host; nobody rewrites it.[9][11]
So a Linux guest kernel module in Rust is realistic **today**, but it lives on top of a C kernel and
we own only the leaf.

---

## 2. Windows kernel/UMDF drivers in Rust

### `windows-drivers-rs` — real, Microsoft-owned, still early
Microsoft's official crate family (`wdk-build`, `wdk-sys`, `wdk`, `wdk-panic`, `wdk-alloc`,
`wdk-macros`) enables Windows driver dev in Rust. `wdk-sys` is bindgen-generated FFI plus hand-
written macro shims; `wdk` is the safe layer.[15][16] **Honest maturity: the README still says
"early stages… not yet recommended for production."**[15]
- **Driver models:** intends to cover **WDM, KMDF, UMDF** (and Win32 services). But **crates.io only
  publishes KMDF 1.33**; other configs (UMDF 2.33, WDM, newer KMDF) require cloning the repo and
  editing the `wdk-sys` `build.rs` config.[16][17] Tested surface is eWDK + KMDF 1.33 / UMDF 2.33 /
  WDM.[17]
- **IddCx: not provided by `windows-drivers-rs`.** No indirect-display-class-extension bindings ship
  in the Microsoft crates. **NEEDS VERIFICATION that this is still true at a later commit**, but it
  was absent as of the sources reviewed.[15][16]
- **Toolchain quirks:** binding generation wants **LLVM 17.0.6** (LLVM 18 has an AArch64 bindgen bug),
  plus `cargo-make` and an eWDK developer prompt.[15] Microsoft recommends **WDK 28000.1761 with
  Visual Studio 2026** for current driver dev generally.[18]

### The proof that Rust IddCx works — outside the Microsoft crates
**`virtual-display-rs` (MolotovCherry)** is a shipping **UMDF indirect-display driver written entirely
in Rust**, using its **own custom bindings** (`wdf-umdf` for WDF + hand-rolled **IddCx bindings**),
**not** `windows-drivers-rs`. It's user-mode (UMDF), x64, Windows 10 2004+.[19] `RustDeskIddDriver`
is another Rust IDD based on Microsoft's official sample.[20] **Implication for us:** a Rust
*virtual display* / IddCx path is *proven*, but if we go IddCx in Rust we will likely maintain our
own bindings rather than lean on the Microsoft crates — more surface we own.

However, an IddCx indirect display is a **display-only** device (it presents monitors and takes
swapchain frames); it is **not** a WDDM 3D render node. A real virtualized GPU on Windows that apps
can `D3D`/Vulkan-render against needs a **WDDM display miniport + user-mode display driver (UMD)**
implementing the **DDI**, which is a far larger, kernel-mode (KMDF/WDM) undertaking with **no Rust
precedent** we found. That is the single biggest Windows-side unknown.

### Signing — this bites hard in 2026
Windows kernel-mode driver trust tightened in the **April 2026** update: Microsoft is **removing
default trust for cross-signed kernel drivers**; by default only **WHCP (Windows Hardware
Compatibility Program)**-signed kernel drivers load, rolled out first in **evaluation/audit mode**,
then enforcement.[21][22][23] Production path for a *kernel-mode* driver therefore means: **EV code-
signing cert + Hardware Dev Center account + HLK testing + Microsoft signature (WHQL or attestation
signing)**.[22][24] Attestation signing is the lighter path (no full HLK lab) but still requires the
EV cert and Microsoft's portal.[22] **A user-mode (UMDF/IddCx) driver has a lighter signing burden
than a KM miniport** — another reason the Windows guest story leans toward keeping as much as
possible in user mode.

---

## 3. How much guest driver is *shared* cross-platform Rust

The reference architectures all converge on the same shape, and it maps cleanly onto a Rust
shared-core/thin-glue split:

- **Venus** serializes **Vulkan** commands into a shared **ring buffer**; guest UMD is a frontend,
  host executes real Vulkan.[25]
- **virtio-gpu** uses a **virtqueue** (shared-memory command ring): guest writes descriptors, kicks,
  host pops and executes.[26][27]
- **DRM native context** mediates the **kernel driver UAPI** (lower-level than Venus/Virgl → less CPU
  overhead, simpler); guest runs the *real* Mesa UMD (radeonsi/radv) and forwards ioctls. Freedreno
  and **AMDGPU are fully upstreamed**, Intel in review, Asahi partial.[28][29]

**What can be one shared `no_std`-capable Rust crate (compiled into both guest kernels and the host):**
- The **wire protocol / serialization** (message framing, versioning, capability negotiation).
- The **command-ring logic** (descriptor layout, producer/consumer indices, fencing/completion,
  flow-control) — pure data-structure code, ideal for Rust + property tests, shared verbatim.
- **Object/handle lifecycle** bookkeeping (resource IDs, refcounts) on the guest side.
- Optionally a **guest user-mode driver (UMD)** shim — though on Linux the UMD is realistically a
  **Mesa (C) Gallium/Vulkan driver**, and on Windows it's a **DDI UMD (C++)**; a *pure-Rust UMD that
  plugs into either graphics stack does not exist** and would be a large net-new effort. Treat the
  UMD as mostly non-shared.

**What stays thin and OS-specific (small, per-OS Rust or C glue):**
- Linux: a **Rust DRM leaf driver** (using the in-tree abstractions) exposing the ring to Mesa.
- Windows: a **KMDF/WDM miniport (or UMDF/IddCx for the display-only path)** that exposes the ring to
  the DDI UMD — this is the hardest, least-Rust-proven glue.

Realistic sharing estimate: **protocol + ring + lifecycle (the "brains") shareable in one Rust crate;
the OS kernel attach points and the graphics-stack UMDs are per-OS and partly non-Rust.**

---

## 4. Host-side backend in Rust

This is where Rust is *strongest* in 2026.

### KVM/QEMU device backend: vfio-user is the seam
The clean, "we own the whole device" path is **vfio-user**: QEMU's **vfio-user *client* was
upstreamed in QEMU 10.1** (Aug 2025; note a late regression means use a build slightly newer than
10.1), letting QEMU talk to an **external userspace PCI device server over a UNIX socket** — arbitrary
PCI devices, not just virtio.[30][31] Server options:
- **`libvfio-user`** (Nutanix, **C**) — the reference server lib; Python bindings; explicitly **not a
  stable API/ABI** yet.[31][32]
- **`rust-vmm/vfio`** (Rust) — ships `vfio-bindings` (FFI to kernel VFIO uapi), `vfio-ioctls` (safe
  wrappers), and a **`vfio-user`** crate of **safe wrappers to implement vfio-user devices**, i.e. the
  **server/device side in Rust**.[33][34] `cloud-hypervisor` (Rust) is already a vfio-user *client*.[31]
  **NEEDS VERIFICATION** whether `rust-vmm`'s `vfio-user` is a fully pure-Rust protocol implementation
  vs. a thin wrapper, but rust-vmm crates are conventionally native Rust, so a **pure-Rust vfio-user
  GPU device backend is plausible today.**

So the host GPU-virtualization backend can be a **standalone Rust process** speaking vfio-user to
QEMU — matching the hard constraint of owning the stack and not adopting an existing QEMU GPU driver.
(Alternative seam: a **vhost-user**-style virtio-gpu backend via `rust-vmm/vhost` — also Rust — if we
choose a virtio device model instead of a raw PCI device.)

### Driving the physical GPU from the Rust host
- **Vulkan:** **`ash`** is the de-facto low-level Rust Vulkan binding; **`vulkano`** is a safe wrapper
  on top of `ash` and notably **supports external-memory import/export** (the mechanism for zero-copy
  guest↔host buffer sharing).[35][36] `vk-mem-rs` wraps AMD's VMA allocator.[36]
- **dma-buf / KMS / scanout:** **`gbm.rs`** (Smithay) binds `libgbm` and integrates with **`drm-rs`**,
  handling buffer allocation and **dma-buf import/export** for zero-copy sharing with display/KMS;
  the `EGL_MESA_image_dma_buf_export` path is the Mesa-side counterpart.[37]
- **CUDA (compute guests):** **`cudarc`** gives safe bindings to the CUDA **driver** API; the
  **Rust-CUDA / `cust`** project covers contexts/runtime interop (recently reworked for soundness).[38][39]
  CUDA remains a **closed C ABI** we bind to, not rewrite — and pulls in NVIDIA licensing considerations
  the core is trying to avoid, so keep it optional/pluggable.

Everything on the host side that isn't the closed vendor userspace (CUDA/driver blob, `libgbm`,
Vulkan ICD) can be **Rust**; those closed pieces are consumed over stable C ABIs via well-maintained
crates.

---

## 5. Concrete per-component Rust-vs-C split

| Component | Language | Confidence / Notes |
|---|---|---|
| **Host device backend** (vfio-user *or* vhost-user server, scheduling, time-slicing policy) | **Rust** | High. `rust-vmm/vfio` `vfio-user` or `vhost`; QEMU 10.1+ client is upstream.[30][33] |
| **Host GPU userspace driver** (Vulkan submit / gbm / CUDA) | **Rust bindings → C ABI** | High. `ash`/`vulkano`, `gbm.rs`+`drm-rs`, `cudarc`.[35][37][38] Vendor ICD/CUDA stay C/closed. |
| **Shared protocol + command-ring + lifecycle crate** | **Rust (`no_std`-clean)** | High. Pure logic; property-testable; reused in guest kernels + host. |
| **Guest Linux kernel shim** (virtual DRM leaf driver) | **Rust** | Medium-High. In-tree DRM Rust abstractions + VGEM-in-Rust precedent, but abstractions still landing (7.2).[6][8] Must be `no_std`. |
| **Guest Linux UMD** (Mesa Gallium/Vulkan driver) | **C (Mesa)** | High. No pure-Rust Mesa driver; write/fork a Mesa driver in C. |
| **Guest Windows display path** (IddCx/UMDF, display-only) | **Rust (custom bindings)** | Medium. Proven by `virtual-display-rs`; own the IddCx/WDF bindings.[19] |
| **Guest Windows render path** (WDDM display miniport + DDI UMD) | **C/C++ (likely), maybe KMDF-Rust** | **Low / biggest unknown.** No Rust WDDM miniport precedent found; DDI is a large closed C++ contract. |
| **NVIDIA GSP firmware** | **Closed blob** | N/A. Consumed, never rewritten.[9] |
| **DRM/KMS/PCI/VFIO cores, Kbuild** | **C (upstream)** | N/A. We're a leaf on top. |

**Reading of the split:** the "brains" (host backend + shared protocol/ring) are cleanly Rust; the
guest *kernel* glue is Rust on Linux and Rust-feasible-but-unproven on Windows; the guest *userspace
graphics driver* (Mesa on Linux, WDDM UMD on Windows) is the stubborn C/C++ mass we either fork or
write from scratch, and it's the same regardless of Rust ambitions.

---

## 6. Toolchain / build story

- **Rust for Linux kernel module:** pin **stable rustc ≥ the kernel's minimum** (1.93 for 7.0, tracks
  Debian).[1][2] Build via the kernel's own Kbuild Rust support; module is `#![no_std]` using `core` +
  kernel `alloc` + `kernel` crate. Requires a matching kernel source tree with `CONFIG_RUST=y` and the
  DRM Rust abstractions enabled (7.2+ for the fuller set).[6][12]
- **Host backend:** ordinary `cargo`, stable Rust, Linux `x86_64-unknown-linux-gnu`. Links `libgbm`,
  Vulkan loader, optionally CUDA at runtime via the binding crates. No kernel constraints; can use
  full `std`, async, etc.
- **Windows guest driver:** install **WDK (28000.1761) + Visual Studio 2026**; for Rust,
  **`windows-drivers-rs`** needs **LLVM 17.0.6** (avoid 18) + `cargo-make` + an **eWDK** developer
  prompt.[15][18] Cross-compiling *to* Windows from a Linux dev box for a *kernel/UMDF* driver is
  effectively unsupported by the WDK's linking/signing flow — build Windows drivers **on Windows**.
  Target `x86_64-pc-windows-msvc`. Expect to **fork/generate UMDF + IddCx bindings yourself**
  (à la `virtual-display-rs`) rather than rely on crates.io.[19]
- **Signing (Windows, production):** budget for an **EV code-signing certificate + Hardware Dev
  Center + Microsoft attestation/WHQL signing**; cross-signed/test-signed kernel drivers are **losing
  default trust starting April 2026**.[21][22] Prefer the **user-mode (UMDF/IddCx)** path where
  possible to reduce signing/attack-surface burden.
- **Shared crate:** keep it `#![no_std]` + `alloc`-optional with a Cargo feature so the *same* code
  compiles into the kernel module, the Windows driver, and the `std` host backend.

## 7. Recommendation for infinigpu

1. **Write the host backend and the shared protocol/command-ring/lifecycle crate in Rust now** — this
   is the low-risk, high-leverage majority of the code, and the ecosystem (`rust-vmm/vfio` vfio-user,
   `ash`/`vulkano`, `gbm.rs`, `cudarc`) is ready.[33][35][37][38]
2. **Adopt vfio-user as the host↔QEMU seam** (QEMU 10.1+ client is upstream), which satisfies "own the
   whole device" without forking QEMU's GPU drivers; keep a vhost-user/virtio-gpu fallback in mind.[30]
3. **Linux guest: a Rust DRM leaf driver is viable** on 7.2+; design the shared crate `no_std`-clean so
   it drops straight in. The Linux UMD is a **Mesa (C)** driver — plan for that C effort explicitly.
4. **Windows guest: bifurcate the risk.** A **UMDF/IddCx display path in Rust is proven** (own the
   bindings). Treat the **WDDM render miniport + DDI UMD as the program's hardest, least-Rust-proven
   component** — prototype it early, expect C/C++, and expect the 2026 signing regime to gate release.
5. **Keep CUDA optional and pluggable** — it's a closed C ABI with licensing baggage the core wants to
   avoid; bind via `cudarc` only for compute-guest features, never on the critical display path.[38]

**Blunt risk ranking (hardest → easiest):** Windows WDDM render miniport ≫ Windows signing/attestation
logistics > Linux Mesa UMD (C) > Linux Rust DRM leaf driver (abstractions still stabilizing) > shared
Rust protocol/ring ≈ host Rust backend (both low risk).

---

## Sources

1. https://botmonster.com/posts/rust-stable-linux-kernel-7/ — Rust stable in Linux 7.0, Rust 1.93 minimum, nightly no longer required
2. https://rust-for-linux.com/rust-version-policy — Debian-anchor Rust version policy
3. https://byteiota.com/rust-stable-linux-kernel-7-driver-developers/ — Linux 7.0 Rust-stable analysis
4. https://9to5linux.com/linux-kernel-7-0-officially-released-this-is-whats-new — Linux 7.0 release (12 April 2026)
5. https://lwn.net/Articles/1050174/ — "The state of the kernel Rust experiment"
6. https://www.phoronix.com/news/Linux-7.2-DRM-Rust — Nova/Tyr/DRM-Rust in 7.2, GPUVM immediate mode, HRT, Airlie "~1 year" comment, VGEM-in-Rust
7. https://rust-for-linux.com/tyr-gpu-driver — Tyr (Arm Mali) Rust GPU driver
8. https://lwn.net/Articles/925500/ — Asahi Lina Rust DRM subsystem abstractions RFC (drv/device/file/gem/mm/ioctl/scheduler)
9. https://rust-for-linux.com/nova-gpu-driver — Nova architecture (nova-core/nova-drm), RTX20+ target, GSP, virtualization-on-top note
10. https://docs.kernel.org/gpu/nova/index.html — Nova NVIDIA GPU driver kernel docs
11. https://kangrejos.com/2025/DRM%20and%20Nova%20GPU%20Driver%20(Update).pdf — Nova/DRM update, GSP boot, Hopper/Blackwell FSP
12. https://rust-for-linux.github.io/docs/kernel/index.html — in-tree `kernel` crate docs (no_std, alloc, abstractions)
13. https://rust-for-linux.github.io/docs/alloc/ — kernel `alloc` (fallible allocation)
14. https://www.bordencastle.com/development/security/linux/2026/02/27/rust-in-linux-kernel-2026.html — 2026 state of kernel Rust, no_std constraints
15. https://github.com/microsoft/windows-drivers-rs — Microsoft windows-drivers-rs crates, maturity, LLVM 17 toolchain, WDM/KMDF/UMDF intent
16. https://crates.io/crates/wdk-sys/0.1.0 — wdk-sys FFI bindings description
17. https://crates.io/crates/wdk-build — wdk-build tested surface (eWDK, KMDF 1.33, UMDF 2.33, WDM), crates.io ships KMDF 1.33 only
18. https://learn.microsoft.com/en-us/windows-hardware/drivers/download-the-wdk — WDK 28000.1761 + Visual Studio 2026
19. https://deepwiki.com/MolotovCherry/virtual-display-rs — pure-Rust UMDF/IddCx driver with custom wdf-umdf + IddCx bindings
20. https://github.com/rustdesk-org/RustDeskIddDriver — Rust IDD driver based on Microsoft's official IDD sample
21. https://techcommunity.microsoft.com/blog/windows-itpro-blog/advancing-windows-driver-security-removing-trust-for-the-cross-signed-driver-pro/4504818 — April 2026 removal of cross-signed kernel driver trust
22. https://learn.microsoft.com/en-us/windows-hardware/drivers/install/kernel-mode-code-signing-policy--windows-vista-and-later- — Windows driver signing policy (EV, attestation, WHQL)
23. https://windowsnews.ai/article/windows-april-2026-kernel-security-overhaul-microsoft-blocks-cross-signed-roots-enforces-whcp.407924 — April 2026 WHCP enforcement, evaluation→enforcement rollout
24. https://learn.microsoft.com/en-us/windows-hardware/drivers/download-the-wdk — WDK download / signing tooling
25. https://docs.mesa3d.org/drivers/venus.html — Venus Vulkan command serialization over virtio-gpu ring
26. https://www.qemu.org/docs/master/system/devices/virtio/virtio-gpu.html — virtio-gpu virtqueue command model
27. https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/ — state of gfx virtualization, native contexts
28. https://www.phoronix.com/news/AMDGPU-VirtIO-Native-Mesa-25.0 — AMDGPU virtio native context merged; native UMD in guest
29. https://lists.gnu.org/archive/html/qemu-devel/2025-06/msg00011.html — virtio-gpu DRM native context v13 (mediates kernel UAPI; freedreno/amdgpu upstream, intel/asahi status)
30. https://movementarian.org/blog/posts/2025-08-27-vfio-user-client-in-qemu/ — vfio-user client upstreamed in QEMU 10.1 (post-10.1 regression fix)
31. https://www.qemu.org/docs/master/system/devices/vfio-user.html — QEMU vfio-user client; arbitrary PCI devices; cloud-hypervisor Rust client
32. https://github.com/nutanix/libvfio-user — libvfio-user C server library (not-stable API, Python bindings)
33. https://github.com/rust-vmm/vfio — rust-vmm vfio crates: vfio-bindings, vfio-ioctls, vfio-user (implement vfio-user devices in Rust)
34. https://github.com/rust-vmm/vfio-ioctls — safe Rust wrappers over VFIO
35. https://github.com/ash-rs/ash — ash low-level Rust Vulkan bindings
36. https://docs.rs/vulkano/latest/vulkano/memory/ — vulkano safe wrapper on ash, external memory import/export
37. https://github.com/Smithay/gbm.rs — libgbm Rust bindings + drm-rs integration, dma-buf import/export
38. https://docs.rs/cudarc/latest/cudarc/ — cudarc safe CUDA driver-API bindings
39. https://lib.rs/crates/cust — cust / Rust-CUDA context+runtime interop
