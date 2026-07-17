// infinigpu-idd — IddCx indirect display driver for the infinigpu virtual GPU (skeleton).
//
// ⚠ VALIDATE ON WINDOWS — this file has NOT been compiled (no WDK on the dev host). It is a
// structurally-correct IddCx skeleton adapted for infinigpu, following the WDK
// IndirectDisplay sample flow. Expect signature/type fixes against your exact WDK headers.
//
// Flow: DriverEntry → EvtDeviceAdd (IddCxDeviceInitialize) → EvtIddCxAdapterInitFinished
// (report the adapter + one monitor) → EvtIddCxMonitorAssignSwapChain (the OS gives us a
// swap-chain of composited desktop frames) → SwapChainProcessor thread acquires each frame,
// copies it to a CPU-readable staging texture, and hands the BGRA bytes to HostLink, which
// (via the KMDF companion) writes them to the device scanout — the Windows analogue of the
// Linux driver's DISPLAY_SCANOUT page-flip.

#include <windows.h>
#include <bugcodes.h>
#include <wudfwdm.h>
#include <wdf.h>
#include <iddcx.h>

#include <dxgi1_5.h>
#include <d3d11_2.h>
#include <wrl.h>

#include <memory>
#include <vector>
#include <atomic>

#include "HostLink.h"

using namespace Microsoft::WRL;

// One fixed mode for the display-only v0. ⚠ the KMDF companion / host should agree on the
// scanout size; a real driver enumerates modes and honors the host's preferred size.
static constexpr UINT kWidth = 1920;
static constexpr UINT kHeight = 1080;
static constexpr UINT kRefreshHz = 60;

// ------------------------------- swap-chain processor -------------------------------

// Owns the render device + the acquire/copy/submit loop for one assigned swap-chain.
class SwapChainProcessor {
public:
    SwapChainProcessor(IDDCX_SWAPCHAIN swapChain, LUID renderAdapter, HANDLE newFrameEvent)
        : m_swapChain(swapChain), m_renderAdapter(renderAdapter), m_newFrameEvent(newFrameEvent) {
        m_terminate.reset(CreateEventW(nullptr, TRUE, FALSE, nullptr));
        m_thread.reset(CreateThread(nullptr, 0, ThreadProc, this, 0, nullptr));
    }

    ~SwapChainProcessor() {
        SetEvent(m_terminate.get());
        if (m_thread) WaitForSingleObject(m_thread.get(), INFINITE);
    }

private:
    struct HandleDeleter { void operator()(HANDLE h) const { if (h && h != INVALID_HANDLE_VALUE) CloseHandle(h); } };
    using UniqueHandle = std::unique_ptr<void, HandleDeleter>;

    static DWORD CALLBACK ThreadProc(LPVOID ctx) {
        reinterpret_cast<SwapChainProcessor*>(ctx)->Run();
        return 0;
    }

    // Create a D3D11 device on the render adapter the OS selected for us.
    bool InitDevice() {
        ComPtr<IDXGIFactory5> factory;
        if (FAILED(CreateDXGIFactory2(0, IID_PPV_ARGS(&factory)))) return false;
        ComPtr<IDXGIAdapter1> adapter;
        for (UINT i = 0; factory->EnumAdapters1(i, &adapter) != DXGI_ERROR_NOT_FOUND; ++i) {
            DXGI_ADAPTER_DESC1 desc{};
            adapter->GetDesc1(&desc);
            if (desc.AdapterLuid.LowPart == m_renderAdapter.LowPart &&
                desc.AdapterLuid.HighPart == m_renderAdapter.HighPart) break;
            adapter.Reset();
        }
        if (!adapter) return false;
        D3D_FEATURE_LEVEL fl;
        return SUCCEEDED(D3D11CreateDevice(adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0,
                                           nullptr, 0, D3D11_SDK_VERSION, &m_device, &fl, &m_context));
    }

    // Copy a composited frame (GPU texture) into a CPU-readable staging texture and map it,
    // returning the BGRA bytes to the caller. Returns false on any D3D error.
    bool ReadBackFrame(ID3D11Texture2D* src, std::vector<uint8_t>& out, UINT& w, UINT& h, UINT& pitch) {
        D3D11_TEXTURE2D_DESC desc{};
        src->GetDesc(&desc);
        w = desc.Width;
        h = desc.Height;

        if (!m_staging || m_stagingW != w || m_stagingH != h) {
            D3D11_TEXTURE2D_DESC sd = desc;
            sd.Usage = D3D11_USAGE_STAGING;
            sd.BindFlags = 0;
            sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
            sd.MiscFlags = 0;
            if (FAILED(m_device->CreateTexture2D(&sd, nullptr, m_staging.ReleaseAndGetAddressOf()))) return false;
            m_stagingW = w;
            m_stagingH = h;
        }
        m_context->CopyResource(m_staging.Get(), src);

        D3D11_MAPPED_SUBRESOURCE map{};
        if (FAILED(m_context->Map(m_staging.Get(), 0, D3D11_MAP_READ, 0, &map))) return false;
        pitch = map.RowPitch;
        out.resize(static_cast<size_t>(pitch) * h);
        memcpy(out.data(), map.pData, out.size());
        m_context->Unmap(m_staging.Get(), 0);
        return true;
    }

    void Run() {
        if (!InitDevice()) return;

        // A real link once the companion exists; the stub keeps the loop observable now.
        std::unique_ptr<HostLink> link;
        {
            auto real = std::make_unique<RealHostLink>();
            link = real->ok() ? std::unique_ptr<HostLink>(std::move(real))
                              : std::unique_ptr<HostLink>(std::make_unique<StubHostLink>());
        }

        std::vector<uint8_t> frame;
        HANDLE waits[] = { m_newFrameEvent, m_terminate.get() };

        for (;;) {
            // Return the previous buffer, then block for the next composited frame.
            IDARG_OUT_RELEASEANDACQUIREBUFFER acquired{};
            HRESULT hr = IddCxSwapChainReleaseAndAcquireBuffer(m_swapChain, &acquired);
            if (hr == E_PENDING) {
                if (WaitForMultipleObjects(2, waits, FALSE, 17 /*~1 frame*/) == WAIT_OBJECT_0 + 1) break;
                continue;
            }
            if (FAILED(hr)) break;

            ComPtr<ID3D11Texture2D> tex;
            if (SUCCEEDED(acquired.MetaData.pSurface->QueryInterface(IID_PPV_ARGS(&tex)))) {
                UINT w = 0, h = 0, pitch = 0;
                if (ReadBackFrame(tex.Get(), frame, w, h, pitch)) {
                    link->SubmitFrame(frame.data(), w, h, pitch);
                }
            }
            acquired.MetaData.pSurface->Release();

            if (WaitForSingleObject(m_terminate.get(), 0) == WAIT_OBJECT_0) break;
        }
    }

    IDDCX_SWAPCHAIN m_swapChain;
    LUID m_renderAdapter;
    HANDLE m_newFrameEvent;
    UniqueHandle m_thread;
    UniqueHandle m_terminate;

    ComPtr<ID3D11Device> m_device;
    ComPtr<ID3D11DeviceContext> m_context;
    ComPtr<ID3D11Texture2D> m_staging;
    UINT m_stagingW = 0, m_stagingH = 0;
};

// Per-device context: the adapter + monitor handles + the active processor.
struct DeviceContext {
    IDDCX_ADAPTER adapter = nullptr;
    IDDCX_MONITOR monitor = nullptr;
    std::unique_ptr<SwapChainProcessor> processor;
};
WDF_DECLARE_CONTEXT_TYPE(DeviceContext);

// ----------------------------------- IddCx callbacks ---------------------------------

static void FillMode(IDDCX_TARGET_MODE& mode, UINT w, UINT h, UINT hz) {
    mode.Size = sizeof(mode);
    mode.TargetVideoSignalInfo.totalSize = { w, h };
    mode.TargetVideoSignalInfo.activeSize = { w, h };
    mode.TargetVideoSignalInfo.vSyncFreq = { hz, 1 };
    mode.TargetVideoSignalInfo.hSyncFreq = { hz * h, 1 };
    mode.TargetVideoSignalInfo.pixelRate = static_cast<UINT64>(w) * h * hz;
    mode.TargetVideoSignalInfo.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
}

_Use_decl_annotations_
static NTSTATUS EvtIddCxAdapterInitFinished(IDDCX_ADAPTER adapter, const IDARG_IN_ADAPTER_INIT_FINISHED* args) {
    auto* ctx = WdfObjectGet_DeviceContext(adapter);
    if (!NT_SUCCESS(args->AdapterInitStatus)) return STATUS_SUCCESS;

    // Report one monitor at connector 0.
    IDDCX_MONITOR_INFO info{};
    info.Size = sizeof(info);
    info.MonitorType = DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED;
    info.ConnectorIndex = 0;
    info.MonitorDescription.Size = sizeof(info.MonitorDescription);
    info.MonitorDescription.Type = IDDCX_MONITOR_DESCRIPTION_TYPE_MONITOR_SPECIFIC; // EDID could go here
    info.MonitorDescription.DataSize = 0;

    IDARG_IN_MONITORCREATE create{};
    create.ObjectAttributes = nullptr;
    create.pMonitorInfo = &info;
    IDARG_OUT_MONITORCREATE created{};
    if (NT_SUCCESS(IddCxMonitorCreate(adapter, &create, &created))) {
        ctx->monitor = created.MonitorObject;
        IDARG_OUT_MONITORARRIVAL arrival{};
        IddCxMonitorArrival(ctx->monitor, &arrival);
    }
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
static NTSTATUS EvtIddCxMonitorGetDefaultModes(IDDCX_MONITOR, const IDARG_IN_GETDEFAULTDESCRIPTIONMODES*,
                                               IDARG_OUT_GETDEFAULTDESCRIPTIONMODES* out) {
    out->DefaultMonitorModeBufferOutputCount = 1;
    if (out->pDefaultMonitorModes) {
        FillMode(reinterpret_cast<IDDCX_TARGET_MODE&>(out->pDefaultMonitorModes[0]), kWidth, kHeight, kRefreshHz);
    }
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
static NTSTATUS EvtIddCxMonitorQueryModes(IDDCX_MONITOR, const IDARG_IN_QUERYTARGETMODES*,
                                          IDARG_OUT_QUERYTARGETMODES* out) {
    out->TargetModeBufferOutputCount = 1;
    if (out->pTargetModes) FillMode(out->pTargetModes[0], kWidth, kHeight, kRefreshHz);
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
static NTSTATUS EvtIddCxAdapterCommitModes(IDDCX_ADAPTER, const IDARG_IN_COMMITMODES*) {
    return STATUS_SUCCESS; // single fixed mode → nothing to reconcile
}

_Use_decl_annotations_
static NTSTATUS EvtIddCxMonitorAssignSwapChain(IDDCX_MONITOR monitor, const IDARG_IN_SETSWAPCHAIN* args) {
    auto* ctx = WdfObjectGet_DeviceContext(monitor);
    ctx->processor = std::make_unique<SwapChainProcessor>(
        args->hSwapChain, args->RenderAdapterLuid, args->hNextSurfaceAvailable);
    return STATUS_SUCCESS;
}

_Use_decl_annotations_
static NTSTATUS EvtIddCxMonitorUnassignSwapChain(IDDCX_MONITOR monitor) {
    auto* ctx = WdfObjectGet_DeviceContext(monitor);
    ctx->processor.reset(); // joins the thread
    return STATUS_SUCCESS;
}

// ------------------------------------ WDF plumbing ----------------------------------

_Use_decl_annotations_
static NTSTATUS EvtDeviceD0Entry(WDFDEVICE, WDF_POWER_DEVICE_STATE) { return STATUS_SUCCESS; }

_Use_decl_annotations_
static NTSTATUS EvtDeviceAdd(WDFDRIVER, PWDFDEVICE_INIT deviceInit) {
    // Let IddCx augment the device-init before we create the WDFDEVICE.
    IDD_CX_CLIENT_CONFIG cfg{};
    IDD_CX_CLIENT_CONFIG_INIT(&cfg);
    cfg.EvtIddCxAdapterInitFinished = EvtIddCxAdapterInitFinished;
    cfg.EvtIddCxMonitorGetDefaultDescriptionModes = EvtIddCxMonitorGetDefaultModes;
    cfg.EvtIddCxMonitorQueryTargetModes = EvtIddCxMonitorQueryModes;
    cfg.EvtIddCxAdapterCommitModes = EvtIddCxAdapterCommitModes;
    cfg.EvtIddCxMonitorAssignSwapChain = EvtIddCxMonitorAssignSwapChain;
    cfg.EvtIddCxMonitorUnassignSwapChain = EvtIddCxMonitorUnassignSwapChain;

    NTSTATUS st = IddCxDeviceInitConfig(deviceInit, &cfg);
    if (!NT_SUCCESS(st)) return st;

    WDF_PNPPOWER_EVENT_CALLBACKS pnp;
    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&pnp);
    pnp.EvtDeviceD0Entry = EvtDeviceD0Entry;
    WdfDeviceInitSetPnpPowerEventCallbacks(deviceInit, &pnp);

    WDF_OBJECT_ATTRIBUTES attrs;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attrs, DeviceContext);
    WDFDEVICE device;
    st = WdfDeviceCreate(&deviceInit, &attrs, &device);
    if (!NT_SUCCESS(st)) return st;

    st = IddCxDeviceInitialize(device);
    if (!NT_SUCCESS(st)) return st;

    // Report the adapter capabilities and kick off async adapter init.
    IDDCX_ADAPTER_CAPS caps{};
    caps.Size = sizeof(caps);
    caps.MaxMonitorsSupported = 1;
    caps.EndPointDiagnostics.Size = sizeof(caps.EndPointDiagnostics);
    caps.EndPointDiagnostics.GammaSupport = IDDCX_FEATURE_IMPLEMENTATION_NONE;
    caps.EndPointDiagnostics.TransmissionType = IDDCX_TRANSMISSION_TYPE_OTHER;
    caps.EndPointDiagnostics.pEndPointFriendlyName = L"infinigpu virtual display";

    IDARG_IN_ADAPTER_INIT init{};
    init.WdfDevice = device;
    init.pCaps = &caps;
    init.ObjectAttributes = &attrs;
    IDARG_OUT_ADAPTER_INIT out{};
    st = IddCxAdapterInitAsync(&init, &out);
    if (!NT_SUCCESS(st)) return st;

    auto* ctx = WdfObjectGet_DeviceContext(device);
    ctx->adapter = out.AdapterObject;
    return STATUS_SUCCESS;
}

extern "C" NTSTATUS DriverEntry(PDRIVER_OBJECT driverObject, PUNICODE_STRING registryPath) {
    WDF_DRIVER_CONFIG config;
    WDF_DRIVER_CONFIG_INIT(&config, EvtDeviceAdd);
    config.DriverPoolTag = 'GPfI';
    return WdfDriverCreate(driverObject, registryPath, WDF_NO_OBJECT_ATTRIBUTES, &config, nullptr);
}
