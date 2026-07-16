//! Minimal PCI configuration space (research/24 §1). QEMU's vfio-user client
//! forwards guest config loads/stores as region reads/writes on the config region;
//! these bytes are authoritative for VEN/DEV/class/subsystem + the MSI-X capability.

use infinigpu_abi::ids;

pub const CONFIG_SPACE_SIZE: usize = 4096;

/// MSI-X capability offset in config space.
const MSIX_CAP: usize = 0x40;
/// Number of MSI-X vectors we expose (vector 0 = device/control, 1..=63 per-context).
pub const MSIX_VECTORS: u16 = 64;

fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// Build the device's config space.
pub fn build() -> Vec<u8> {
    let mut c = vec![0u8; CONFIG_SPACE_SIZE];

    put_u16(&mut c, 0x00, ids::PCI_VENDOR_ID);
    put_u16(&mut c, 0x02, ids::PCI_DEVICE_ID);
    // Status (0x06): bit4 = capabilities list present.
    put_u16(&mut c, 0x06, 0x0010);
    c[0x08] = ids::PCI_REVISION_ID;
    // Class code: prog-if / subclass / base-class = 0x00 / 0x80 / 0x03 → 0x038000
    // (Display, "other" — secondary, does not claim the boot framebuffer).
    c[0x09] = (ids::PCI_CLASS_DISPLAY_OTHER & 0xff) as u8;
    c[0x0a] = ((ids::PCI_CLASS_DISPLAY_OTHER >> 8) & 0xff) as u8;
    c[0x0b] = ((ids::PCI_CLASS_DISPLAY_OTHER >> 16) & 0xff) as u8;
    c[0x0e] = 0x00; // header type 0 (single-function endpoint)
    put_u16(&mut c, 0x2c, ids::PCI_SUBSYSTEM_VENDOR_ID);
    put_u16(&mut c, 0x2e, ids::PCI_SUBSYSTEM_DEVICE_ID);
    c[0x34] = MSIX_CAP as u8; // capabilities pointer

    // MSI-X capability (id 0x11).
    c[MSIX_CAP] = 0x11;
    c[MSIX_CAP + 1] = 0x00; // next capability = none
                            // Message Control: table size field = N-1.
    put_u16(&mut c, MSIX_CAP + 2, MSIX_VECTORS - 1);
    // Table:  BAR1 (BIR=1), offset 0x0000  →  (offset & !7) | BIR.
    put_u32(&mut c, MSIX_CAP + 4, 1);
    // PBA:    BAR1 (BIR=1), offset 0x2000.
    put_u32(&mut c, MSIX_CAP + 8, 0x2000 | 1);

    c
}
