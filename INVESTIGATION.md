# OpenRM cuInit failure under cloud-hypervisor

## Context

This branch captures the 2026-06-12 investigation of CUDA initialization failures
with NVIDIA's OPEN kernel module stack under cloud-hypervisor. It is not intended
as a production fix.

Observed symptom:

- Guest uses NVIDIA OPEN kernel module 580.159.03 with GSP enabled.
- Eight H100 GPUs are passed through with VFIO.
- `cuInit()` returns `CUDA_ERROR_NOT_INITIALIZED` (`rc=3`) under
  cloud-hypervisor.
- The same guest disk and kernel work under QEMU.
- The closed/proprietary NVIDIA module works under cloud-hypervisor.
- `nvidia-smi`, NVML, and fabric manager are functional.
- Enabling or disabling vIOMMU does not change the failure.

RM tracing showed every ioctl succeeding, including embedded NV status values of
zero. The call stream matches QEMU for 758 calls. The divergence appears after the
per-GPU `NV2080_CTRL_CMD_BUS_*` (`0x2080182b`) and `NVLINK_STATUS`
(`0x20803002`) queries: under cloud-hypervisor, libcuda issues
`GPU_DETACH_IDS`; under QEMU, it proceeds to `THIRD_PARTY_P2P REGISTER_VA_SPACE`.

The working assumption is therefore that OpenRM userspace rejects data returned by
a successful query. The root cause is not proven yet.

## Static findings

### VFIO endpoint config space is not pure passthrough

`pci/src/vfio.rs` builds a cloud-hypervisor-owned PCI config-space model around
the VFIO device. Standard capabilities, BARs, MSI/MSI-X, and config-space patches
are handled locally before guest reads are returned.

One concrete difference from direct passthrough is in
`VfioCommon::parse_extended_capabilities()`: cloud-hypervisor currently masks the
following endpoint extended capabilities by patching their capability ID to
`NullCapability`:

- ARI: `AlternativeRoutingIdentificationInterpretation`
- ReBAR: `ResizeableBar`
- SR-IOV: `SingleRootIoVirtualization`

Why this matters: OpenRM userspace is known to have queried bus and link data
successfully immediately before deciding to detach the GPUs. If QEMU forwards one
of these endpoint capabilities while cloud-hypervisor hides it, libcuda may be
making a policy decision from different, successful config-space query data.

Why this is not changed by default: these capabilities can expose guest-visible
controls that cloud-hypervisor may not virtualize fully or safely. Exposing them
unconditionally would change the device contract for every VFIO user, not just
the H100/OpenRM case.

### Root topology differs from the QEMU setup

This tree creates a simple PCI host bridge with `PciRoot::new(None)` in
`pci/src/bus.rs`. `vmm/src/pci_segment.rs` creates one such root bridge per PCI
segment. There is no QEMU-style `pcie-root-port` object in this code path and no
local root-port PCIe capability block where a small atomic-operations bit toggle
can be applied.

Why this matters: the failing deployment differed from QEMU in root-complex
shape. QEMU used a single-segment topology with `pxb`/root ports, while
cloud-hypervisor used multiple PCI segments for the passed-through GPUs. NVIDIA
userspace may include PCI domain, parent bridge, or root-port capability data in
the query data that leads to the detach decision.

Why no root-port experiment is added here: cloud-hypervisor does not have a
root-port device model in this branch to patch narrowly. Adding synthetic root
ports would be a topology change, not a one-bit experiment, and would mix several
hypotheses at once.

### Segment/domain exposure is a live-hardware comparison item

cloud-hypervisor's multi-segment layout likely exposes PCI domains other than
domain 0 for the eight GPUs. The QEMU setup from the reproduction used a
different topology and may have exposed a different domain/segment view.

Why this matters: the RM trace points at bus and link queries rather than failed
ioctls. Domain/segment values are plausible inputs to userspace topology policy,
especially with NVLink and third-party P2P registration.

Why this branch does not change segment handling: collapsing or remapping PCI
segments would affect guest-visible topology and resource allocation globally. It
should be tested only after config-space forwarding has been isolated.

### AMX CPUID remains a separate hypothesis

The reproduction noted that cloud-hypervisor lacked AMX CPUID bits in the tested
configuration. This branch does not modify CPU feature exposure.

Why this is left separate: the observed divergence follows GPU bus/link queries,
not CPU feature ioctls, and changing CPUID would not directly validate the VFIO
config-space differences found in this code path.

## Experimental toggle

This branch adds one off-by-default experiment:

```text
CH_VFIO_OPENRM_EXPOSE_FILTERED_EXT_CAPS=1
```

When set to a truthy value, cloud-hypervisor skips its current masking of VFIO
endpoint ARI, ReBAR, and SR-IOV extended capabilities. Empty, `0`, `false`, `no`,
and `off` are treated as false.

Hypothesis tested: OpenRM userspace rejects CUDA init because cloud-hypervisor
hides an endpoint extended capability that QEMU forwards, causing successful
NVIDIA bus/link query data to differ.

Expected test value:

- If `cuInit()` starts working with only this toggle enabled, the next step is to
  isolate which of ARI, ReBAR, or SR-IOV matters and implement a narrower, safer
  virtualized exposure.
- If `cuInit()` still fails, the endpoint extended-capability mask is probably
  not sufficient, and root topology, PCI domain/segment values, root-port
  capabilities, BAR/ReBAR details, or CPUID differences remain higher-value
  candidates.

## Validation plan for real hardware

Run all comparisons on the same host, guest disk, kernel, NVIDIA driver, fabric
manager version, and GPU set used in the reproduction.

1. Boot baseline cloud-hypervisor and capture guest PCI config:

   ```sh
   lspci -Dnnvvxxxx -s <gpu-bdf> > ch-baseline-gpu.txt
   lspci -Dnnvvxxxx -t > ch-baseline-tree.txt
   ```

2. Boot cloud-hypervisor with only the new toggle enabled and capture the same
   data:

   ```sh
   CH_VFIO_OPENRM_EXPOSE_FILTERED_EXT_CAPS=1 cloud-hypervisor ...
   lspci -Dnnvvxxxx -s <gpu-bdf> > ch-extcaps-gpu.txt
   lspci -Dnnvvxxxx -t > ch-extcaps-tree.txt
   ```

3. Boot the known-working QEMU configuration and capture:

   ```sh
   lspci -Dnnvvxxxx -s <gpu-bdf> > qemu-gpu.txt
   lspci -Dnnvvxxxx -t > qemu-tree.txt
   ```

4. Diff the captures for:

   - Endpoint extended capability list and raw offsets.
   - ARI, ReBAR, and SR-IOV presence.
   - BAR aperture sizes and ReBAR capability contents.
   - Parent bridge/root-port PCIe capabilities.
   - DevCap2/DevCtl2 AtomicOp completer/requester bits.
   - ACS, ATS, PRI, and PASID capabilities if present.
   - PCI domain/segment and bus numbering.

5. Run `cuInit()` with RM tracing enabled and compare whether cloud-hypervisor
   still emits `GPU_DETACH_IDS` after the `NV2080` bus and `NVLINK_STATUS`
   queries or proceeds to `THIRD_PARTY_P2P REGISTER_VA_SPACE`.

## Decision log

- Added only the endpoint extended-capability toggle because it maps to a known
  cloud-hypervisor code path, has no effect unless explicitly enabled, and tests
  one plausible CH/QEMU difference.
- Did not alter default VFIO capability filtering because those masks likely
  protect unsupported guest control paths.
- Did not add a root-port DevCap2/AtomicOp toggle because this branch does not
  model QEMU-style root ports. A synthetic root-port implementation would be a
  larger topology experiment, not a small diagnostic switch.
- Did not change PCI segment/domain assignment because that is a global topology
  decision and should be tested after endpoint config-space differences are
  isolated.
- Did not modify CPUID/AMX exposure because it is a separate hypothesis and the
  strongest trace clue points at PCI/GPU topology query data.
