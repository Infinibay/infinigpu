# 02 — GPU Sharing Taxonomy: What Can Actually Share One NVIDIA A5000 Across Many VMs

**Scope:** A blunt, sourced taxonomy of GPU virtualization/sharing techniques, scored against infinigpu's hard constraints: **share one physical GPU 1→N**, **no per-VM proprietary license**, **works on NVIDIA RTX A5000 (Ampere GA102)**, **fully ownable by us**, **Windows AND Linux guests first-class**.

## Bottom line (read this first)

There are exactly **two** ways to make one NVIDIA GPU serve many VMs: (a) **hardware/driver-mediated partitioning** — NVIDIA vGPU/GRID via SR-IOV+mdev — which the A5000 *does* support but **only under NVIDIA's proprietary `vgpu-kvm` host driver plus per-VM DLS licensing**; or (b) **host-level API mediation / paravirtualization** — the host owns the real GPU with the ordinary free driver and each guest gets a *synthetic* GPU whose commands are marshaled to a host multiplexer. Only class (b) is license-free and ownable. **Everything license-free and NVIDIA-compatible collapses onto the paravirtualization class.** Its Achilles' heel is exactly infinigpu's requirement: **Windows guests**, which the existing open implementations (virtio-gpu/VirGL/Venus) do not serve.

---

## 1. VFIO passthrough (1:1) — the non-sharing baseline

VFIO (`vfio-pci`) is safe *direct assignment* of a whole physical PCI device to one VM using the IOMMU (Intel VT-d / AMD-Vi) for DMA isolation; the device must sit in its own IOMMU group and the host is booted with `intel_iommu=on`/`amd_iommu=on` ([ArchWiki OVMF passthrough](https://wiki.archlinux.org/title/PCI_passthrough_via_OVMF), [Red Hat RHV GPU passthrough](https://docs.redhat.com/en/documentation/red_hat_virtualization/4.3/html/setting_up_an_nvidia_gpu_for_a_virtual_machine_in_red_hat_virtualization/proc_nvidia_gpu_passthrough_nvidia_gpu_passthrough)). The guest gets ~bare-metal performance because it talks to the card almost directly.

**Why it does not share:** assignment is *exclusive*. Once a GPU is bound to `vfio-pci` and handed to a VM, the host loses it and no other VM can touch it — it is one device, one guest, for the lifetime of that VM ([cloudrift host setup](https://www.cloudrift.ai/blog/host-setup-for-qemu-kvm-gpu-passthrough-with-vfio-on-linux)). With 2× A5000 you can passthrough to at most **2** VMs, statically. This is the anti-pattern our project exists to beat. VFIO stays in scope only as a **transport primitive** (we may still use IOMMU/VFIO plumbing internally on the host).

## 2. SR-IOV on GPUs — vendor reality

SR-IOV lets a PCIe device advertise lightweight **Virtual Functions (VFs)** that a hypervisor assigns to guests as if they were separate devices. On GPUs, SR-IOV is the *hardware substrate* for sharing, but the VFs are useless without a vendor control-plane driver on the Physical Function.

- **AMD MxGPU** (since 2016) uses SR-IOV to hardware-partition the GPU across VMs; supported on specific SKUs (FirePro S7150, Radeon Pro V340/V520, Instinct MI series via the virt driver) ([Open-IOV GPU Support](https://open-iov.org/index.php/GPU_Support), [AMD Instinct MxGPU getting started](https://instinct.docs.amd.com/projects/virt-drv/en/latest/userguides/Getting_started_with_MxGPU.html)). Notably AMD's is the closest thing to a *without-per-VM-license* SR-IOV path, but it is AMD hardware — not our A5000.
- **Intel Data Center Flex** (Flex 140/170) exposes SR-IOV VFs for VDI on vSphere 8.0U2; consumer **Arc discrete GPUs do NOT support SR-IOV** ([William Lam / xcp-ng forum](https://xcp-ng.org/forum/topic/8614/intel-flex-gpu-with-sr-iov-for-gpu-accelarated-vdis)).
- **NVIDIA:** SR-IOV on Ampere+ is *only* the enabling step for vGPU. On the **RTX A5000** you must first switch the board to "DC/graphics mode" with NVIDIA's **Display Mode Selector** tool and enable SR-IOV (Proxmox runs `pve-nvidia-sriov@ALL.service` / NVIDIA's `sriov-manage`), which yields **24 VFs** for the A5000 ([Proxmox NVIDIA vGPU wiki](https://pve.proxmox.com/wiki/NVIDIA_vGPU_on_Proxmox_VE), [NVIDIA A5000 vGPU forum](https://forums.developer.nvidia.com/t/rtx-a5000-vgpu-support/273584)). **Crucially, those VFs come alive only when the proprietary `vgpu-kvm` host driver is loaded** — see §3.

**Verdict:** SR-IOV *is* present on the A5000 silicon, but there is **no open/license-free way to drive it**. It is a locked door for which NVIDIA holds the only key (§7).

## 3. Mediated devices (VFIO-mdev) + NVIDIA vGPU/GRID — the licensed time-slicer

**Mechanism.** `mdev` is a Linux framework where a vendor driver on the physical device carves out virtual "mediated" devices, each backed by a VFIO handle a guest attaches to. NVIDIA vGPU builds on this: on Volta and earlier the mdev sits on the PF; on **Ampere+ each mdev is bound to an SR-IOV VF** ([Open-IOV](https://open-iov.org/index.php/GPU_Support)). Newer NVIDIA drivers (kernel 6.8+) moved from generic `mdev` to a **vendor-specific VFIO variant driver** ([cloudrift GPU virtualization](https://www.cloudrift.ai/blog/gpu-virtualization-qemu-kvm-nvidia-amd)).

**How it time-slices.** Multiple vGPUs on one physical GPU share the engines by **temporal multiplexing** governed by a scheduler with three policies — **Best Effort** (round-robin, maximizes utilization, prone to "noisy neighbor"), **Equal Share** (equal slices among active vGPUs), **Fixed Share** (fixed slices sized by vGPU profile) ([NVIDIA AI Enterprise scheduling docs](https://docs.nvidia.com/ai-enterprise/release-8/latest/infra-software/vgpu/features/scheduling.html), [VxWorld time-slicing policies](https://vxworld.co.uk/2025/06/30/understanding-nvidia-vgpu-time-slicing-policies-best-effort-vs-equal-share-vs-fixed-share/)). This is real hardware context-switching of the GPU between guests — exactly the "intelligent time-slicing, not static partitioning" infinigpu wants — but it lives **inside NVIDIA's closed firmware/driver** and we cannot see or own it.

**The two disqualifiers:**
1. **Proprietary host driver.** vGPU requires NVIDIA's `NVIDIA-Linux-x86_64-<ver>-vgpu-kvm.run` host package (DKMS). **vGPU is *not* supported by NVIDIA's open-source `open-gpu-kernel-modules`, nor by `nouveau`** — the open modules explicitly defer vGPU to the proprietary "vGPU Host Package," and NVIDIA docs instruct disabling `nouveau` for vGPU ([NVIDIA open-gpu-kernel-modules](https://github.com/NVIDIA/open-gpu-kernel-modules), [NVIDIA driver kernel-modules docs](https://docs.nvidia.com/datacenter/tesla/driver-installation-guide/kernel-modules.html), [Proxmox wiki](https://pve.proxmox.com/wiki/NVIDIA_vGPU_on_Proxmox_VE)).
2. **Per-VM licensing.** Each guest vGPU must check out a license from a **Delegated License Service (DLS)**; unlicensed guests are throttled/degraded. This is a paid NVIDIA vGPU subscription (vPC/vWS/vApps) ([Proxmox wiki](https://pve.proxmox.com/wiki/NVIDIA_vGPU_on_Proxmox_VE), [NVIDIA vGPU user guide](https://docs.nvidia.com/vgpu/16.0/grid-vgpu-user-guide/index.html)).

Both violate infinigpu's constraints (b) no per-VM license and (a) we own the stack. **vGPU/GRID is off-limits as our core**, full stop. (Also note **MIG** — hardware-partitioned instances — exists **only on A100/A30/H100/H200/B200, not GA102/A5000** ([NVIDIA docs](https://docs.nvidia.com/ai-enterprise/release-8/latest/infra-software/vgpu/features/scheduling.html)), so it is doubly irrelevant to us.)

### 3b. `vgpu_unlock` — the gray-market bypass (why it is still not our core)

The community `vgpu_unlock` project makes consumer/pro cards impersonate a vGPU-capable datacenter SKU. It does this with an `LD_PRELOAD` Rust shim (`vgpu_unlock-rs`) that intercepts ioctl/`mmap` calls to the NVIDIA driver and rewrites the PCI device-ID checks; separate projects (FastAPI-DLS) fake the license server ([DualCoder/vgpu_unlock](https://github.com/DualCoder/vgpu_unlock), [KrutavShah/vGPU_LicenseBypass](https://github.com/KrutavShah/vGPU_LicenseBypass)). Two fatal problems for us:

- It **still requires the proprietary NVIDIA GRID/`vgpu-kvm` host driver** — it only *tricks* that driver, it does not replace it ([vgpu_unlock README](https://github.com/DualCoder/vgpu_unlock)). So we would not own the mediation layer; we'd be reverse-engineering around a closed binary that NVIDIA can (and does) break each driver release.
- **Ampere support is "work in progress"/unstable** — the README lists Maxwell/Pascal/Volta/Turing; **the A5000 (GA102/Ampere) is not a first-class, reliable target** ([vgpu_unlock README](https://github.com/DualCoder/vgpu_unlock)).

This is a fragile, legally/ToS-gray hack riding closed firmware — the opposite of "100% custom, ownable." **Reject as core; keep only as reference for how NVIDIA's mdev boundary behaves.**

## 4. API remoting / paravirtualization — the ownable class

Here the **host** keeps exclusive ownership of the real GPU (using the *ordinary, free* NVIDIA driver — the standard GeForce/RTX/CUDA driver is **not** the licensed vGPU product). Each guest sees a **synthetic GPU device**; a guest-side driver **serializes graphics/compute API calls and ships them over a virtio/VM-bus transport** to a **host-side renderer/multiplexer** that replays them against the one real driver, interleaving many guests. This is genuine 1→N sharing done in *software we can write*.

### 4a. virtio-gpu + VirGL / Venus / native context (the Linux reference stack)

- **VirGL** remotes **OpenGL/GLES** (guest emits TGSI, host translates to GLSL and re-compiles — work happens *twice*, so it is heavy under concurrency) ([Mesa VirGL](https://docs.mesa3d.org/drivers/virgl.html), [Collabora 2025 state of gfx virt](https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/)).
- **Venus** remotes **Vulkan** as a thin command-serialization transport (Vulkan 1.3 as of early 2025), and importantly **runs the host renderer in an isolated process** so a Vulkan crash doesn't take down the VMM; **NVIDIA's proprietary driver 570.86+ is a tested Venus host** ([Mesa Venus](https://docs.mesa3d.org/drivers/venus.html), [Collabora](https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/)).
- **Native context / vDRM** (e.g. AMDGPU native context merged in Mesa 25.0) forwards the *native* UAPI for near-native performance, but needs guest drivers matched to host hardware and is still maturing ([Phoronix AMDGPU native context](https://www.phoronix.com/news/AMDGPU-VirtIO-Native-Mesa-25.0), [Collabora](https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/)).

Unlike passthrough, **"the host and all VM guests can access the host GPU simultaneously"** here ([Collabora](https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/)) — that is the multiplexing we want, license-free, on NVIDIA silicon.

### 4b. CUDA remoting (rCUDA-style)

rCUDA intercepts CUDA API calls in the guest and forwards them via RPC (sockets/InfiniBand) to a server owning the GPU, letting many clients share one physical GPU ([rCUDA / Wikipedia](https://en.wikipedia.org/wiki/RCUDA), [GPGPU virtualization survey](https://vfast.org/journals/index.php/VTCS/article/download/521/548)). Strengths: easy setup, portable, broad model support. Weaknesses that bite us: it is **compute-only (no display/WDDM presentation path)**, performance is **bounded by transport latency/serialization under many concurrent clients**, and the wrapper library must be **chased against every CUDA release** ([survey](https://vfast.org/journals/index.php/VTCS/article/download/521/548)). Useful as a *pattern* for the compute path, not a whole-desktop VDI solution.

### 4c. The Windows weakness (this is the crux for infinigpu)

Every open paravirtualization stack above is **Linux/Android-guest only**. VirGL explicitly does **not** target Windows Direct3D guests; guests must run a Linux kernel with virtio + Mesa ([Mesa VirGL](https://docs.mesa3d.org/drivers/virgl.html), [Collabora](https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/)). The Mesa Venus docs describe **only Linux/Android guests** — **no Windows guest support** ([Mesa Venus](https://docs.mesa3d.org/drivers/venus.html)). Since Infinibay must serve **Windows desktops as first-class**, adopting virtio-gpu as-is is a dead end for half our fleet.

The **reference architecture** for the Windows side is Microsoft's **WDDM GPU Paravirtualization (GPU-P)**: the guest has no real KMD — a *Virtual Render Device* loads `Dxgkrnl`, which **thunks DirectX/WDDM calls and marshals them over the VM bus (≤128KB messages) to the host partition** that owns the physical GPU ([Microsoft Learn — GPU paravirtualization](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-paravirtualization)). This is *exactly* the API-remoting model, proven for Windows guests — but it is Hyper-V-specific and, for NVIDIA, Microsoft's implementation still leans on the vendor vGPU driver on the host ([Argon Systems GPU-P](https://argonsys.com/microsoft-cloud/library/gpu-partitioning-in-windows-server-2025-hyper-v/)). **infinigpu's Windows-first challenge is to build our own WDDM paravirtual guest driver + host multiplexer on KVM without the vGPU driver** — the single largest engineering risk in this whole space.

## 5. Time-sliced GPU scheduling in general

"Time-slicing" simply means the GPU's engines are context-switched between tenants over time rather than physically split. It appears in three tiers: **(1) firmware/hardware** context switch driven by a closed scheduler (NVIDIA vGPU's Best-Effort/Equal/Fixed policies — great, but not ours, §3); **(2) driver-level** cooperative multiplexing (NVIDIA MPS-style, or a *host multiplexer we write* that arbitrates submission queues from many guests into the one host driver context); **(3) API-call arbitration** in the paravirtualization renderer (round-robin/weighted fair queueing across guest command streams). infinigpu's schedulable surface is **tier 2/3**: we cannot preempt inside NVIDIA's silicon at will (no open access to the hardware runlist scheduler), so our fairness/QoS lives in **how the host multiplexer orders and rate-limits guest submissions** — a policy engine we design (weighted fair share, priority, per-department quotas). This is achievable but is *cooperative* time-slicing (preemption granularity limited by how the driver yields), not hardware-guaranteed isolation.

## 6. Comparison matrix

| Technique | Shares 1→N? | License-free? | Works on A5000 (GA102)? | Fully ownable by us? | Windows + Linux guests? | Verdict for infinigpu |
|---|---|---|---|---|---|---|
| **VFIO passthrough (1:1)** | ❌ exclusive | ✅ | ✅ | ✅ (uses stock VFIO) | ✅ both | Baseline only; can't share. Reuse as host primitive. |
| **SR-IOV (generic)** | ✅ (hardware VFs) | depends on vendor | VFs exist but **need vgpu-kvm** | ❌ (closed PF driver on NVIDIA) | ✅ both | Locked on NVIDIA; no open key. |
| **AMD MxGPU / Intel Flex SR-IOV** | ✅ | ✅-ish (no per-VM license) | ❌ wrong vendor | partial | ✅ both | Not our hardware; proves SR-IOV *can* be license-free elsewhere. |
| **NVIDIA vGPU/GRID (mdev+SR-IOV)** | ✅ time-sliced | ❌ **per-VM DLS license** | ✅ (24 VFs, DC mode) | ❌ proprietary driver+fw | ✅ both | **Rejected** — violates license + ownership. |
| **vgpu_unlock hack** | ✅ | ✅ (bypasses license) | ⚠️ Ampere WIP/unstable | ❌ rides closed driver | ✅ both | **Rejected** — fragile, gray, not ownable. |
| **MIG** | ✅ hard partitions | needs vGPU sw | ❌ **A100/A30/H100 only** | ❌ | n/a | Irrelevant to GA102. |
| **virtio-gpu VirGL/Venus (paravirt)** | ✅ simultaneous | ✅ | ✅ (host uses stock driver) | ✅ **open, we can extend** | ⚠️ **Linux/Android only today** | **Core candidate** — must add Windows. |
| **CUDA remoting (rCUDA-style)** | ✅ | ✅ | ✅ | ✅ | ⚠️ compute-only, no display | Compute-path pattern, not whole VDI. |
| **WDDM GPU-P paravirt (Windows ref)** | ✅ | ✅ *if* we replace vendor driver | ✅ (host stock driver) | ⚠️ we must build guest KMD | ✅ (the Windows half) | **Reference to replicate for Windows.** |

## 7. What is reachable on NVIDIA non-vGPU silicon WITHOUT proprietary mediation

Stated bluntly, on a plain **RTX A5000 with the ordinary free driver (or the open kernel modules / nouveau)**:

- **No hardware GPU partitioning is available to us.** MIG doesn't exist on GA102; SR-IOV VFs won't enumerate/function without the proprietary `vgpu-kvm` driver; the open kernel modules and nouveau **do not implement vGPU/mdev host functionality at all** ([NVIDIA open-gpu-kernel-modules](https://github.com/NVIDIA/open-gpu-kernel-modules), [NVIDIA kernel-modules docs](https://docs.nvidia.com/datacenter/tesla/driver-installation-guide/kernel-modules.html)). There is **no open door to NVIDIA's hardware time-slicer.**
- **What *is* reachable:** the host runs the standard free driver, owns the physical GPU as a single client, and **we build the sharing in software above it** — a synthetic guest GPU + host multiplexer that time-shares that one driver context across many guests (the paravirtualization class of §4). This is license-free (no vGPU subscription), NVIDIA-compatible, and 100% ours.

In one sentence: **on NVIDIA consumer/pro silicon without proprietary mediation, GPU sharing is only reachable as host-side API/paravirtual mediation — never as hardware partitioning.**

## 8. Conclusion — the class infinigpu should adopt

**infinigpu's core must be the API-remoting / paravirtualization class (§4): a custom virtio-GPU-style paravirtual device + a host-side GPU multiplexer, with our own guest drivers.** Rationale:

1. It is the **only** license-free, NVIDIA-A5000-compatible, fully-ownable way to get true 1→N sharing (matrix §6). vGPU/GRID and vgpu_unlock are both eliminated by the no-license + ownership constraints.
2. The host runs the **ordinary free driver**, so we sidestep the entire vGPU licensing regime while still using NVIDIA's fast native driver for actual execution.
3. There is a real, current (2025) open reference stack to study and borrow protocol design from — **virtio-gpu + virglrenderer (VirGL/Venus/native-context)** on the Linux side, and **Microsoft WDDM GPU-P** on the Windows side.

**The decisive risk, and where our engineering must concentrate:** every existing open paravirtualization implementation is **Linux-guest-only**; Windows first-class means we must **build a Windows guest driver (WDDM paravirtual miniport whose `Dxgkrnl`/D3D calls marshal to our host multiplexer)** — the hardest, least-precedented part of the project ([Mesa Venus/VirGL Linux-only](https://docs.mesa3d.org/drivers/venus.html); [Microsoft GPU-P as the Windows blueprint](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-paravirtualization)). Time-slicing fairness will be **cooperative, scheduled in our host multiplexer** (weighted fair share / quotas), because NVIDIA's hardware runlist scheduler is not open to us (§5, §7). Recommend a phased plan: prove the paravirtual transport + host multiplexer with **Linux guests first** (leveraging Venus/virtio-gpu learnings), then invest heavily in the **Windows WDDM guest driver** as the make-or-break milestone.

## Sources

- Red Hat RHV — GPU device passthrough (VFIO 1:1): https://docs.redhat.com/en/documentation/red_hat_virtualization/4.3/html/setting_up_an_nvidia_gpu_for_a_virtual_machine_in_red_hat_virtualization/proc_nvidia_gpu_passthrough_nvidia_gpu_passthrough
- ArchWiki — PCI passthrough via OVMF (IOMMU/VFIO): https://wiki.archlinux.org/title/PCI_passthrough_via_OVMF
- Cloudrift — Host setup for QEMU/KVM GPU passthrough with VFIO: https://www.cloudrift.ai/blog/host-setup-for-qemu-kvm-gpu-passthrough-with-vfio-on-linux
- Cloudrift — GPU virtualization with VFIO, NVIDIA AI Enterprise, AMD SR-IOV: https://www.cloudrift.ai/blog/gpu-virtualization-qemu-kvm-nvidia-amd
- Open-IOV — GPU Support matrix (mdev/SR-IOV/vGPU per GPU): https://open-iov.org/index.php/GPU_Support
- AMD Instinct — Getting started with MxGPU (SR-IOV): https://instinct.docs.amd.com/projects/virt-drv/en/latest/userguides/Getting_started_with_MxGPU.html
- xcp-ng forum — Intel Flex GPU with SR-IOV for VDI: https://xcp-ng.org/forum/topic/8614/intel-flex-gpu-with-sr-iov-for-gpu-accelarated-vdis
- Proxmox VE Wiki — NVIDIA vGPU on Proxmox (vgpu-kvm driver, SR-IOV enable, 24 VFs on A5000, DLS licensing): https://pve.proxmox.com/wiki/NVIDIA_vGPU_on_Proxmox_VE
- NVIDIA Developer Forums — RTX A5000 vGPU support (DC mode / Display Mode Selector): https://forums.developer.nvidia.com/t/rtx-a5000-vgpu-support/273584
- NVIDIA Virtual GPU Software User Guide 16.0 (licensing): https://docs.nvidia.com/vgpu/16.0/grid-vgpu-user-guide/index.html
- NVIDIA AI Enterprise — vGPU scheduling policies (Best Effort / Equal / Fixed; MIG hardware list): https://docs.nvidia.com/ai-enterprise/release-8/latest/infra-software/vgpu/features/scheduling.html
- VxWorld — Understanding NVIDIA vGPU time-slicing policies: https://vxworld.co.uk/2025/06/30/understanding-nvidia-vgpu-time-slicing-policies-best-effort-vs-equal-share-vs-fixed-share/
- NVIDIA open-gpu-kernel-modules (vGPU not supported by open modules): https://github.com/NVIDIA/open-gpu-kernel-modules
- NVIDIA Driver Installation Guide — Kernel Modules (open vs proprietary, vGPU host package): https://docs.nvidia.com/datacenter/tesla/driver-installation-guide/kernel-modules.html
- DualCoder/vgpu_unlock (still needs GRID driver; Ampere WIP): https://github.com/DualCoder/vgpu_unlock
- KrutavShah/vGPU_LicenseBypass: https://github.com/KrutavShah/vGPU_LicenseBypass
- Collabora — The state of GFX virtualization using virglrenderer (2025): https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/
- Mesa 3D docs — Venus (Vulkan over virtio-gpu; Linux/Android guests): https://docs.mesa3d.org/drivers/venus.html
- Mesa 3D docs — VirGL (OpenGL over virtio-gpu; no Windows D3D): https://docs.mesa3d.org/drivers/virgl.html
- Phoronix — AMDGPU VirtIO Native Context merged (Mesa 25.0): https://www.phoronix.com/news/AMDGPU-VirtIO-Native-Mesa-25.0
- Microsoft Learn — GPU paravirtualization (WDDM GPU-P, Dxgkrnl marshaling over VM bus): https://learn.microsoft.com/en-us/windows-hardware/drivers/display/gpu-paravirtualization
- Argon Systems — GPU Partitioning in Windows Server 2025 Hyper-V: https://argonsys.com/microsoft-cloud/library/gpu-partitioning-in-windows-server-2025-hyper-v/
- rCUDA — Wikipedia: https://en.wikipedia.org/wiki/RCUDA
- GPGPU Virtualization Techniques: A Comparative Survey (API remoting pros/cons): https://vfast.org/journals/index.php/VTCS/article/download/521/548
