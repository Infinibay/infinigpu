# 10 — Per-VM GPU Isolation, Fault Containment & DoS (no hardware isolation)

**Scope:** how a single shared A5000 (GA102) driven by userspace API-remoting can be made
safe in a *multi-tenant* deployment (Infinibay departments + RBAC + per-VM fail-closed
firewalls) **without** MIG / SR-IOV / vGPU / per-context IOMMU. The GPU sharing must not
become the weakest link in an otherwise fail-closed platform. This doc is deliberately
adversarial toward the Wave-1 "software isolation is enough" assumption.

## Bottom line up front — verdict: PARTIALLY-CONFIRMED

Software isolation holds up **for confidentiality/integrity and for the common fault case**,
but **not** for worst-case availability. Three things are real and defensible:

1. The arbiter/renderer *can* and *must* run as a separate, jailed process **per VM**, out of
   the VMM address space — this is exactly what Venus + crosvm + `vhost-device-gpu` already do
   ([Collabora 2025](https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/),
   [crosvm README](https://github.com/google/crosvm/blob/main/README.md)).
2. **Most** GPU faults triggered by a bad guest command stream are contained to the offending
   context/channel by NVIDIA's own *Robust Channels* recovery — the GPU stays healthy for other
   tenants ([Xid field guide](https://www.abhik.ai/articles/nvidia-xid-errors),
   [Xid reporting / KernelRc](https://deepwiki.com/eunomia-bpf/gpu_ext-kernel-modules/6.2-error-reporting-(xid))).
3. VRAM, ring, and rate DoS are all software-boundable via arbiter admission control.

**But** there is an *irreducible shared-fault-domain residue*: a class of severe faults
(Xid 79 fallen-off-bus, 45 preemptive-removal, 62 microcontroller-halt, 48 double-bit ECC,
119/120 GSP RPC timeout) forces a **full-device reset** that takes down **every** tenant, and
`nvidia-smi -r` confirms a GPU reset is device-wide and requires *all* processes to be killed
first ([nvidia-smi docs](https://docs.nvidia.com/deploy/nvidia-smi/index.html)). A malicious
guest that can provoke an engine hang (Xid 43-class) may be able to *escalate* into that
domain. **You cannot fully prevent one tenant from causing a device-wide availability event in
software** — this must be an explicitly accepted, monitored residual risk, not an unstated gap.

## 1. Process topology & blast radius — one backend *per VM*

The single most important structural decision. Two axes: (a) in-VMM vs out-of-VMM, (b) one
arbiter-per-host vs one-backend-per-VM.

**Out-of-VMM is settled.** Venus is *the only* virglrenderer context type that runs the renderer
in an isolated host process precisely so that "Vulkan crashing on the host won't take down the
whole VMM" ([Collabora 2025](https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/));
virglrenderer even ships a *render-server* proxy mode (`VIRGL_RENDERER_RENDER_SERVER`) to push
decode/replay into a separate address space. crosvm runs every virtio device in its own
Minijail-jailed child — namespaces (pivot_root/PID/user/net), strict seccomp-BPF syscall
allowlists, all capabilities dropped ([crosvm README](https://github.com/google/crosvm/blob/main/README.md));
`vhost-device-gpu`'s stated goal is "to isolate the device backend from the VMM to reduce the
attack surface" ([modernizing virtio-gpu](https://speakerdeck.com/ennael/modernizing-virtio-gpu-a-rust-powered-approach-with-vhost-device-gpu)).
Whether we take the vfio-user seam or our own virtio-style vhost-user device (the Wave-1
open question), **the arbiter is a separate userspace process either way** — the security
posture is identical, so the seam choice can be made on other grounds.

**One-per-host vs one-per-VM — pick per-VM.** A single host arbiter serving all VMs is simpler
for global VRAM accounting and uses one Vulkan instance, but a Rust panic, an `abort()` in shared
decode code, or an unhandled fatal Vulkan error in *any* one VM's path takes the whole process
down — i.e. its blast radius is *all tenants*, defeating the point. One backend process per VM
makes the blast radius of an arbiter bug/crash/compromise exactly one tenant, and it mirrors
Infinibay's existing per-VM TAP + per-VM nftables firewall model. **Recommended topology:** a
thin, privileged **GPU broker** (one per host) that holds the device policy/quota ledger and
admits/denies context creation, plus **one unprivileged, jailed replay process per VM**, each
opening its *own* NVIDIA `VkDevice`/context. The broker never touches guest data; the per-VM
process never sees another tenant's memory (separate Vulkan contexts = separate GPU virtual
address spaces). Cost: more RAM and per-context overhead than a shared process — acceptable.

## 2. GPU fault containment — the critical multi-tenant risk

**Does one guest's malicious/buggy stream take down all guests?** *Usually no, sometimes yes* —
and the boundary is what matters.

NVIDIA's *Robust Channels* subsystem (`KernelRc` in the open kernel modules) owns both the
GPU watchdog and channel error recovery; when a channel enters an error state the driver
retrieves the allocating process and attributes the fault to it
([Xid/KernelRc](https://deepwiki.com/eunomia-bpf/gpu_ext-kernel-modules/6.2-error-reporting-(xid))).
The fault taxonomy splits cleanly (Xid field guide, corroborated by NVIDIA's Xid docs — the
field guide is a **secondary** source, so treat exact per-Xid actions as NEEDS VERIFICATION):

- **Contained to the offending context (GPU stays up for others):** Xid 13 graphics-engine
  exception ("the specific kernel that faulted is terminated, but other kernels on different SMs
  may continue"), Xid 31 MMU page fault ("the CUDA context that triggered the fault is
  immediately killed"), Xid 43 reset-channel and Xid 94 contained-ECC. NVIDIA docs class Xid 13
  and 43 as "usually caused by user jobs" that "do not impact the health of the GPU."
- **Device-wide — takes down *all* tenants:** Xid 79 fallen-off-bus, Xid 45 preemptive removal
  ("only way back is a full system reboot or PCIe hot-reset"), Xid 62 microcontroller halt, Xid 48
  double-bit ECC ("corrupts data across all contexts"), Xid 119/120 GSP RPC timeout.
  ([Xid field guide](https://www.abhik.ai/articles/nvidia-xid-errors),
  [AWS Xid guide](https://repost.aws/knowledge-center/ec2-linux-troubleshoot-xid-errors)).

**The MPS anti-pattern — do not use it.** NVIDIA Multi-Process Service multiplexes all clients
into *one shared context*, so "a fatal fault from one client can destroy the shared context and
terminate **all** co-running clients, regardless of which client caused the fault"
([MPS fault-resilience study, arXiv 2605.26461](https://arxiv.org/pdf/2605.26461)). MPS trades
fault isolation for SM concurrency — the exact wrong trade for multi-tenant security. **Each VM
gets its own context, never a shared MPS context.** (This also means we accept context-switch
time-slicing rather than concurrent SM sharing — a performance cost, not a security one.)

**The escalation worry (my inference — NEEDS VERIFICATION):** a guest whose shader deliberately
hangs a compute/graphics engine can drive Xid 43-class "GPU stopped processing," and the driver's
engine reset "affects all workloads waiting on that engine." Ampere supports instruction-level
compute preemption and the RC watchdog should reset the hung *channel* rather than the device,
but if engine reset does not cleanly recover, a hostile tenant could turn a contained fault into a
device event. **We must assume this is possible** and design the recovery path (below) around it.
This is the single strongest refutation of "software isolation is sufficient."

**Recovery design:** treat `VK_ERROR_DEVICE_LOST` as a *per-VM* signal first — tear down only that
VM's context, mark the VM's GPU "faulted," notify Infinibay over the existing Socket.IO bridge, and
leave other tenants running. Monitor `dmesg`/NVML Xid stream out-of-band (DCGM-style); on any
full-reset-class Xid, quarantine the device, stop admitting new GPU contexts, drain/kill all per-VM
replay processes, run the device reset, and only then re-admit — with the *provoking* VM flagged
for the department's audit trail.

## 3. Guest-crash resource reaping (ResourceTracker teardown)

Every guest GPU handle (buffer, image, pipeline, descriptor set, memory, fence, swapchain) has a
host twin. gfxstream's `ResourceTracker` is exactly this guest-handle → host-handle map, and it is
"the bulk of the code" ([gfxstream](https://github.com/google/gfxstream)). Reaping rules for the
per-VM process:

- The per-VM process **owns** its ResourceTracker; on guest disconnect/crash/VM-destroy the process
  destroys *all* host Vulkan objects it created, frees device memory, closes dma-bufs, and exits.
  Because the process is per-VM, the cleanest reaping primitive is **kill the process** — the OS
  reclaims file descriptors, mmaps, and the Vulkan driver tears down the context, guaranteeing no
  orphaned host GPU state survives a crashed guest. This is strictly more reliable than in-process
  bookkeeping and is why per-VM topology also wins on reaping.
- Beware head-of-line hazards in the teardown path: the helix.ml multi-desktop stack froze *all*
  contexts on a single global `renderer_blocked` semaphore, and FIFO command queues blocked
  blob-unmaps behind later commands ([helix.ml](https://blog.helix.ml/p/gpu-virtualization-architecture-for)).
  Per-VM processes + gfxstream's 1:1 thread model avoid one VM's teardown stalling another.
- The broker must reconcile: on per-VM process exit (expected or crashed), the broker releases that
  VM's VRAM/quota reservation so a crash-looping tenant can't leak its whole department's budget.
  This mirrors Infinibay's existing VM crash-reconciliation on backend startup.

## 4. DoS containment (ring flood / VRAM / runaway shaders)

- **Command-ring flood.** virtqueue backpressure is inherent: when the ring (256 entries in 2D,
  up to 1024 in 3D) fills, the guest blocks — it cannot force unbounded host work by spamming, it
  can only fill its *own* ring ([virtio-gpu ring behavior](https://blog.helix.ml/p/gpu-virtualization-architecture-for)).
  The arbiter must still cap *decode* work per drain cycle and never allocate host memory
  proportional to an unbounded guest-supplied count. A malicious driver "may craft an infinite
  descriptor chain causing a DoS" — the backend must "detect such a loop and fail the request"
  ([Red Hat: hardening virtio](https://www.redhat.com/en/blog/hardening-virtio-emerging-security-usecases)).
- **Unbounded VRAM.** There is *no* hardware VRAM isolation without MIG, so the only wall is
  **arbiter admission control**: track each VM's device-memory allocations and **refuse** any
  `vkAllocateMemory` past its per-VM quota (return out-of-device-memory to the guest). Without this
  a single greedy tenant starves the whole department. Quotas are per-VM, summed and capped per
  department.
- **Runaway / infinite-loop shaders.** Vulkan has *no* host-enforced runtime bound;
  `VK_KHR_shader_terminate_invocation` is app-controlled, not a watchdog. Defenses: (a) submit each
  VM's work at a low `VK_EXT_global_priority` tier so the driver can preempt it (Ampere has
  instruction-level compute preemption); (b) token-bucket / deficit throttle in the arbiter metered
  by GPU timestamps, so a VM's *submission rate* is capped regardless of shader content; (c) rely on
  NVIDIA's RC watchdog to kill a genuinely hung channel — then handle the resulting DEVICE_LOST as a
  per-VM fault (§2). A hard host-side "kill this VM's GPU work after N ms" is only reliably available
  by killing the per-VM replay process.
- **Fairness DoS.** One tenant monopolizing GPU time is a soft DoS; the arbiter's per-VM priority +
  token bucket (drm_sched-style fairness reference, doc 06) is the mitigation. Head-of-line blocking
  in a *shared* decode thread is itself a self-inflicted DoS — another reason for per-VM processes.

## 5. Validating the untrusted command stream (the arbiter is the attack surface)

The decoder replays a hostile byte stream into a privileged process that holds real GPU access —
a classic confused-deputy target. virglrenderer has a CVE history of exactly this class
(memory-init and buffer-overflow guest→host bugs, e.g.
[CVE-2022-0175](https://github.com/advisories/GHSA-28q3-mx7c-4cc3),
[USN-5309-1](https://ubuntu.com/security/notices/USN-5309-1)). Rules:

- **Treat every ring word as hostile.** Validate every descriptor, offset, length, and enum;
  reject reserved/illegal metadata and unsupported combinations; "validate every request… instead
  of assuming that the virtio driver can follow the spec"
  ([Red Hat](https://www.redhat.com/en/blog/hardening-virtio-emerging-security-usecases)).
- **Never trust a guest-supplied handle or pointer.** Every guest handle must resolve through the
  *per-VM* ResourceTracker to a host object the arbiter itself created; a handle that isn't in the
  map is a hard error, not a host pointer. This prevents a guest from forging references to another
  tenant's or the host's resources.
- **Force robustness on the host device.** Create the host `VkDevice` with `robustBufferAccess`
  (and `VK_EXT_robustness2`) **forced on** regardless of what the guest requests, so out-of-bounds
  shader/descriptor access is clamped rather than corrupting host memory
  ([robustness2](https://registry.khronos.org/vulkan/)).
- **Shrink the surface with Rust + a jail.** crosvm/`vhost-device-gpu` chose Rust for the decoder
  specifically for memory safety — this directly retires most of the virgl C-decoder CVE class and
  supports the Wave-1 "Rust host backend" call. Wrap it in seccomp-BPF (allow only the NVIDIA ioctls
  + minimal syscalls), no filesystem beyond `/dev/nvidia*`, no network, dropped capabilities, and
  ideally a **dedicated unprivileged uid per VM**. A decoder RCE then yields only that jail.

## 6. Fail-closed posture & Infinibay per-department policy mapping

Consistent with Infinibay's fail-closed firewalls and fail-closed HMAC: **no GPU unless policy
grants it.** Concretely, add a GPU policy layer that reuses the existing department/RBAC/Prisma
plumbing:

- **RBAC gate:** attaching a virtual GPU to a VM is a permissioned resolver action, checked in
  `backend` before `infinization` spawns anything — same layering as VM create.
- **Department = quota domain** (new Prisma fields, enforced by the broker): `gpuEnabled`
  (default *false* — fail-closed), `vramCapMB` per VM and per department, `priorityTier`
  (→ `VK_EXT_global_priority`), `maxConcurrentGpuVMs`, `submissionRateTokens`. Backend writes these
  to Postgres; the broker enforces admission at context creation and continuously at runtime.
- **Spawn model:** `infinization` spawns the per-VM jailed replay process alongside the VM's TAP +
  nftables firewall, exactly as it wires per-VM networking today; teardown reaps it with the VM.
- **Fail-closed defaults everywhere:** broker refuses context creation if quota/policy checks fail
  or the device is quarantined; if the host lacks the expected NVIDIA/Vulkan capability the VM
  starts *without* GPU (control-plane-only) rather than with an unmediated device — mirroring the
  `INFINIZATION_BRIDGE_CONNTRACK_MODE=fail` posture.
- **Auditability:** DEVICE_LOST events, quota denials, and Xid faults flow to the same Socket.IO /
  Postgres path as VM health, so a tenant that repeatedly faults the GPU is visible per department.

## Top 3 security must-dos for the MVP

1. **One jailed replay process per VM, its own NVIDIA context, never MPS.** Out of the VMM address
   space, seccomp+namespace+dropped-caps, unprivileged (ideally per-VM uid). This single decision
   contains (a) an arbiter/decoder compromise, (b) a host Vulkan crash, and (c) crashed-guest
   resource reaping (kill-the-process reaping) — all to one tenant. MPS is banned because its shared
   context is a shared fault domain.
2. **Treat the command stream as hostile and force host-side robustness.** Validate every
   descriptor/handle/offset against the per-VM ResourceTracker, reject unknown handles, force
   `robustBufferAccess`/`robustness2` on the host device, detect infinite descriptor chains — and
   write the decoder in Rust to retire the virgl C-decoder CVE class.
3. **Fail-closed admission control + fault quarantine.** GPU off by default; per-department VRAM
   caps, priority tier, submission-rate cap, and max-GPU-VMs enforced *before* context creation and
   at runtime; handle `VK_ERROR_DEVICE_LOST` as a per-VM fault (tear down one VM, keep the rest);
   monitor the Xid stream and, on any full-reset-class Xid, quarantine the device and flag the
   provoking VM. **Document the device-wide-reset residual risk explicitly** — it is the one thing
   software cannot fully close on this hardware.

## Sources

- Collabora — state of GFX virtualization / Venus process isolation (2025): https://www.collabora.com/news-and-blog/blog/2025/01/15/the-state-of-gfx-virtualization-using-virglrenderer/
- Mesa Venus driver docs: https://docs.mesa3d.org/drivers/venus.html
- crosvm README (process-per-device, Minijail/seccomp): https://github.com/google/crosvm/blob/main/README.md
- Modernizing virtio-gpu with vhost-device-gpu (Rust, backend-out-of-VMM): https://speakerdeck.com/ennael/modernizing-virtio-gpu-a-rust-powered-approach-with-vhost-device-gpu
- NVIDIA Xid error field guide (secondary — per-Xid recovery, NEEDS VERIFICATION): https://www.abhik.ai/articles/nvidia-xid-errors
- NVIDIA Xid errors (official): https://docs.nvidia.com/deploy/xid-errors/archive/index.html
- AWS — troubleshoot NVIDIA Xid errors: https://repost.aws/knowledge-center/ec2-linux-troubleshoot-xid-errors
- Xid reporting / Robust Channels (KernelRc) from open-gpu-kernel-modules: https://deepwiki.com/eunomia-bpf/gpu_ext-kernel-modules/6.2-error-reporting-(xid)
- nvidia-smi docs (GPU reset is device-wide): https://docs.nvidia.com/deploy/nvidia-smi/index.html
- Characterization-Guided GPU Fault Resilience in NVIDIA MPS (shared-context fault domain), arXiv 2605.26461: https://arxiv.org/pdf/2605.26461
- gfxstream (ResourceTracker, 1:1 threading): https://github.com/google/gfxstream
- Helix — GPU virtualization for multi-desktop containers (renderer_blocked head-of-line): https://blog.helix.ml/p/gpu-virtualization-architecture-for
- Red Hat — Hardening virtio for emerging security use cases (untrusted-input rules): https://www.redhat.com/en/blog/hardening-virtio-emerging-security-usecases
- virglrenderer CVE-2022-0175 (guest→host memory): https://github.com/advisories/GHSA-28q3-mx7c-4cc3
- Ubuntu USN-5309-1 — virglrenderer vulnerabilities: https://ubuntu.com/security/notices/USN-5309-1
- VK_KHR_shader_terminate_invocation: https://registry.khronos.org/VulkanSC/specs/1.0-extensions/man/html/VK_KHR_shader_terminate_invocation.html
- Khronos Vulkan registry (robustness2, global_priority): https://registry.khronos.org/vulkan/
