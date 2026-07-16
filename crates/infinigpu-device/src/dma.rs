//! Guest IOVA → host-VA translation (research/24 §4). QEMU (launched with
//! `memory-backend-memfd,share=on`) sends one or a few `DMA_MAP`s carrying the
//! guest-RAM memfd as an `SCM_RIGHTS` fd; we `mmap` each and record the interval.
//! Translation is then a pointer add — zero copy, no socket round-trip.
//!
//! Every lookup is **fail-closed and bounds-checked**: a hostile guest handing us
//! an IOVA/length outside a mapped interval gets `None`, never an out-of-bounds host
//! read (ERRATA #7).

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;

struct Mapping {
    size: usize,
    host: *mut u8,
    // The mmap keeps the pages alive independently, but holding the File keeps the
    // fd valid and makes ownership explicit; dropped in `Drop`.
    _fd: File,
}

/// A set of non-overlapping guest-physical → host-virtual mappings.
#[derive(Default)]
pub struct DmaTable {
    maps: BTreeMap<u64, Mapping>,
}

impl DmaTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Map `[iova, iova+size)` to `mmap(fd, offset, size)`.
    pub fn map(&mut self, iova: u64, offset: u64, size: u64, fd: File) -> io::Result<()> {
        if size == 0 {
            return Ok(());
        }
        // SAFETY: standard shared mmap of a file descriptor; MAP_FAILED is checked.
        let host = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as libc::size_t,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                offset as libc::off_t,
            )
        };
        if host == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        self.maps.insert(
            iova,
            Mapping {
                size: size as usize,
                host: host as *mut u8,
                _fd: fd,
            },
        );
        Ok(())
    }

    /// Unmap a single interval by its base IOVA.
    pub fn unmap(&mut self, iova: u64) {
        if let Some(m) = self.maps.remove(&iova) {
            // SAFETY: `host`/`size` came from a successful `mmap` above.
            unsafe { libc::munmap(m.host as *mut libc::c_void, m.size) };
        }
    }

    pub fn clear(&mut self) {
        let keys: Vec<u64> = self.maps.keys().copied().collect();
        for k in keys {
            self.unmap(k);
        }
    }

    /// Resolve `[addr, addr+len)` to a host pointer, or `None` if it isn't fully
    /// inside a single mapping. Fail-closed; never returns a partial/OOB pointer.
    fn resolve(&self, addr: u64, len: usize) -> Option<*mut u8> {
        let (&base, m) = self.maps.range(..=addr).next_back()?;
        let end = addr.checked_add(len as u64)?;
        if end <= base.checked_add(m.size as u64)? {
            // SAFETY: (addr - base) < size, checked above.
            Some(unsafe { m.host.add((addr - base) as usize) })
        } else {
            None
        }
    }

    /// Copy `buf.len()` bytes from guest memory at `addr`. Returns false if the
    /// range is not fully mapped.
    pub fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        match self.resolve(addr, buf.len()) {
            // SAFETY: `resolve` bounds-checked the full range against one mapping.
            Some(p) => {
                unsafe { std::ptr::copy_nonoverlapping(p, buf.as_mut_ptr(), buf.len()) };
                true
            }
            None => false,
        }
    }

    /// Copy `buf.len()` bytes into guest memory at `addr`. Returns false if unmapped.
    pub fn write(&self, addr: u64, buf: &[u8]) -> bool {
        match self.resolve(addr, buf.len()) {
            // SAFETY: bounds-checked; the mapping is shared and writable.
            Some(p) => {
                unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), p, buf.len()) };
                true
            }
            None => false,
        }
    }

    pub fn read_u32(&self, addr: u64) -> Option<u32> {
        let mut b = [0u8; 4];
        self.read(addr, &mut b).then(|| u32::from_le_bytes(b))
    }

    pub fn write_u32(&self, addr: u64, val: u32) -> bool {
        self.write(addr, &val.to_le_bytes())
    }

    pub fn len(&self) -> usize {
        self.maps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.maps.is_empty()
    }
}

impl Drop for DmaTable {
    fn drop(&mut self) {
        self.clear();
    }
}
