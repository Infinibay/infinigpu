// HostLink — the handoff from the IddCx (user-mode) display driver to the KMDF companion
// that owns the infinigpu PCI device. A captured desktop frame (tightly-packed BGRA,
// top-down) is submitted here; the companion copies it into the device's scanout
// framebuffer and issues a DISPLAY_SCANOUT command (see crates/infinigpu-abi and
// guest/linux/infinigpu.c — the Windows side speaks the same wire contract).
//
// ⚠ VALIDATE ON WINDOWS — not compiled. The companion (infinigpu-kmdf) is not written yet,
// so only StubHostLink is functional (it proves the IddCx capture loop runs end-to-end
// without a companion). RealHostLink is the shape the companion IOCTL should take.
#pragma once

#include <windows.h>
#include <cstdint>
#include <cstring>
#include <vector>

class HostLink {
public:
    virtual ~HostLink() = default;
    // Submit one captured frame: `bgra` is `pitch * height` bytes, rows top-down, each
    // pixel B,G,R,A. Returns false on a link error (the caller may drop the frame).
    virtual bool SubmitFrame(const uint8_t* bgra, uint32_t width, uint32_t height, uint32_t pitch) = 0;
};

// Bring-up stub: accepts and drops every frame. Lets the IddCx pipeline be brought up and
// observed (frame counter / timing) before the KMDF companion exists.
class StubHostLink : public HostLink {
public:
    bool SubmitFrame(const uint8_t*, uint32_t, uint32_t, uint32_t) override { return true; }
};

// The private device interface the KMDF companion should expose.
#define INFINIGPU_COMPANION_SYMLINK L"\\\\.\\infinigpu"
#define IOCTL_INFINIGPU_SUBMIT_FRAME \
    CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_IN_DIRECT, FILE_WRITE_ACCESS)

// Header prepended to the frame bytes in the IOCTL input buffer. `format` matches
// infinigpu_abi::wire::format (BGRA/XRGB default) so the host reads the pixels correctly.
#pragma pack(push, 1)
struct InfinigpuFrameHeader {
    uint32_t width;
    uint32_t height;
    uint32_t pitch;
    uint32_t format;
};
#pragma pack(pop)

// Real link to the KMDF companion via IOCTL_INFINIGPU_SUBMIT_FRAME. ⚠ needs the companion.
class RealHostLink : public HostLink {
public:
    RealHostLink() {
        m_device = CreateFileW(INFINIGPU_COMPANION_SYMLINK, GENERIC_WRITE,
                               FILE_SHARE_READ | FILE_SHARE_WRITE, nullptr, OPEN_EXISTING,
                               FILE_ATTRIBUTE_NORMAL, nullptr);
    }
    ~RealHostLink() override {
        if (m_device != INVALID_HANDLE_VALUE) CloseHandle(m_device);
    }
    bool ok() const { return m_device != INVALID_HANDLE_VALUE; }

    bool SubmitFrame(const uint8_t* bgra, uint32_t width, uint32_t height, uint32_t pitch) override {
        if (m_device == INVALID_HANDLE_VALUE) return false;
        // Input buffer = [header][pixels]. METHOD_IN_DIRECT would normally use an MDL for
        // the pixels; kept simple here (a real companion may prefer a shared section for
        // zero-copy of a full-screen frame). ⚠ tune for your companion.
        const uint32_t bytes = pitch * height;
        InfinigpuFrameHeader hdr{ width, height, pitch, /*BGRA*/ 0 };
        // Two writes would need FILE_FLAG_OVERLAPPED plumbing; instead pack into one buffer.
        static thread_local std::vector<uint8_t> buf;
        buf.resize(sizeof(hdr) + bytes);
        memcpy(buf.data(), &hdr, sizeof(hdr));
        memcpy(buf.data() + sizeof(hdr), bgra, bytes);
        DWORD returned = 0;
        return DeviceIoControl(m_device, IOCTL_INFINIGPU_SUBMIT_FRAME, buf.data(),
                               (DWORD)buf.size(), nullptr, 0, &returned, nullptr) != FALSE;
    }

private:
    HANDLE m_device = INVALID_HANDLE_VALUE;
};
