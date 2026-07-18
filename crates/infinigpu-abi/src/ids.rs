//! PCI identity, device magic, and ABI version — the load-bearing "who am I"
//! fields the guest driver matches on to bind (research/24 §1).

/// PCI Vendor ID. `0x1B36` is the Red Hat / QEMU virtual-device vendor — a safe
/// development-time placeholder. Apply for a real PCI-SIG vendor ID before GA.
pub const PCI_VENDOR_ID: u16 = 0x1B36;

/// PCI Device ID for "infinigpu proto gen-0".
///
/// NOTE (ERRATA #6): the design docs' original placeholder `0x0100` collides with
/// QEMU's QXL device (`1b36:0100`), whose in-tree driver would try to bind ours.
/// We use `0x0110`, an unallocated DEV under this vendor, until a real ID exists.
pub const PCI_DEVICE_ID: u16 = 0x0110;

pub const PCI_REVISION_ID: u8 = 0x01;
pub const PCI_SUBSYSTEM_VENDOR_ID: u16 = 0x1B36;
pub const PCI_SUBSYSTEM_DEVICE_ID: u16 = 0x0001;

/// Class code when infinigpu is a *secondary* (non-VGA) display adapter — the
/// default; it does not fight over the boot framebuffer. Base class `0x03` = Display.
pub const PCI_CLASS_DISPLAY_OTHER: u32 = 0x0003_8000; // 0x038000
/// Class code when infinigpu is the guest's *sole/primary* adapter.
pub const PCI_CLASS_DISPLAY_VGA: u32 = 0x0003_0000; // 0x030000

/// `DEV_MAGIC` register value `0x49475055` (research/24 §2): its big-endian bytes
/// spell ASCII "IGPU". The guest driver compares the register against this u32.
pub const DEV_MAGIC: u32 = 0x4947_5055;

pub const ABI_MAJOR: u16 = 0;
/// Minor-version history (each step purely additive — a peer that doesn't negotiate the new caps
/// never sends or accepts the new messages). v2 added the 2D-accel wire (`DISPLAY_SCANOUT_DAMAGE`,
/// `ScanoutPresentDamaged`, `DISPLAY_ACCEL`/`CAP_DISPLAY_2D`). v3 added the cursor-plane wire
/// (`CursorUpdate`, `cursor_flags`, `caps::CURSOR_PLANE`, reserving `msg_type::MEDIA_REGION`). v4
/// added the blob-backing wire (`AttachBacking`, `MemEntry` — the `RESOURCE_ATTACH_BACKING` payload
/// the PR4 ring drainer records into the per-VM `ResourceTable`).
pub const ABI_MINOR: u16 = 4;

/// Packed `ABI_VERSION` register value (`major << 16 | minor`).
pub const fn abi_version() -> u32 {
    ((ABI_MAJOR as u32) << 16) | (ABI_MINOR as u32)
}
