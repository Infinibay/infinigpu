/* Cross-language ABI conformance: the C view (generated from infinigpu-abi by
 * cbindgen) must have the exact byte layout the Rust side asserts. Compile with:
 *   cc -std=c11 -I guest/include guest/include/abi_conformance.c -o /tmp/abiconf
 * A build failure here means the Rust ABI and the generated C header drifted.
 * Mirrors infiniservice's cross-language HMAC test. */
#include <stddef.h>
#include "infinigpu_abi.h"

_Static_assert(sizeof(struct Descriptor) == 32, "Descriptor size");
_Static_assert(offsetof(struct Descriptor, seqno) == 16, "Descriptor.seqno offset");

_Static_assert(sizeof(struct MsgHeader) == 8, "MsgHeader size");

_Static_assert(sizeof(struct SubmitCmd) == 40, "SubmitCmd size");
_Static_assert(offsetof(struct SubmitCmd, seqno) == 16, "SubmitCmd.seqno offset");
_Static_assert(offsetof(struct SubmitCmd, out_fence) == 32, "SubmitCmd.out_fence offset");

_Static_assert(sizeof(struct ClearPresent) == 32, "ClearPresent size");
_Static_assert(offsetof(struct ClearPresent, rgba) == 8, "ClearPresent.rgba offset");
_Static_assert(offsetof(struct ClearPresent, scanout_addr) == 24, "ClearPresent.scanout_addr offset");

_Static_assert(sizeof(struct ScanoutPresent) == 24, "ScanoutPresent size");
_Static_assert(offsetof(struct ScanoutPresent, scanout_addr) == 16, "ScanoutPresent.scanout_addr offset");

/* ScanoutPresentDamaged is a ScanoutPresent superset: same prefix + scanout_addr@16,
 * with a trailing damage rect (dx,dy,dw,dh). The shared prefix MUST stay byte-identical. */
_Static_assert(sizeof(struct ScanoutPresentDamaged) == 40, "ScanoutPresentDamaged size");
_Static_assert(offsetof(struct ScanoutPresentDamaged, scanout_addr) == 16, "ScanoutPresentDamaged.scanout_addr offset");
_Static_assert(offsetof(struct ScanoutPresentDamaged, dx) == 24, "ScanoutPresentDamaged.dx offset");
_Static_assert(offsetof(struct ScanoutPresentDamaged, dh) == 36, "ScanoutPresentDamaged.dh offset");

_Static_assert(sizeof(struct ResourceCreateBlob) == 24, "ResourceCreateBlob size");
_Static_assert(sizeof(struct SetScanoutBlob) == 24, "SetScanoutBlob size");
_Static_assert(sizeof(struct ResourceFlush) == 24, "ResourceFlush size");

int main(void) { return 0; }
