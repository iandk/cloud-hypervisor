# NVSwitch-behind-root-port firmware-init failure: root cause + fix

> ## TL;DR — full 8×H100 NVLS bring-up needed THREE fixes (cuInit=0 achieved 2026-06-16)
> 1. **PCIe root ports** (this branch): OpenRM refuses GPUs on a flat root bus → cuInit=3. Necessary
>    but not sufficient.
> 2. **NVSwitch BAR placement** (this doc, §2–3, **implemented + validated**): the NVSwitch's 64-bit
>    *non-prefetchable* BAR0 was placed >4 GiB; a bridge can't forward non-prefetchable >4 GiB, and
>    CH's `move_bar` rejected the guest's relocation. Fix = select the MMIO aperture **by address**,
>    not by the 64-bit flag, in `move_bar` **and** `free_bars`. Result: SXid 10008 gone, Fabric
>    Completed ×8.
> 3. **Guest-side `nvidia_uvm uvm_disable_hmm=1`** (NOT a CH change): after 1+2, cuInit *still* = 3.
>    Cause = open-driver UVM HMM path: `uvm_pmm_devmem_init()` → `request_free_mem_region()` tries to
>    carve a ~80 GB `MEMORY_DEVICE_PRIVATE` window **per GPU** out of host phys-addr space; in an
>    800 GB guest no hole fits → `-ERANGE` → GPU never registers with nvidia-uvm → cuInit=3.
>    Workaround `uvm_disable_hmm=1` (open-gpu-kernel-modules #797/#947) → **cuInit=0, 8 devices**.
>    Harmless for NCCL/NVLS (HMM only backs `cudaMallocManaged`). Persisted in the guest at
>    `/etc/modprobe.d/nvidia-uvm-hmm.conf`. A heavier *hypervisor-side* alternative would be to lay
>    out the guest phys-addr map so 8×~80 GB device-private holes fit (not needed if managed memory
>    isn't required).
>
> Dead ends ruled out on hardware (don't re-chase): the 802 code (broken-NVSwitch transient, not
> progress); the fabric/NVLink path (cuInit=3 persists with `NvLinkDisable=1`); `Addressing Mode: HMM`
> as an *addressing/ATS* problem (H100 has no ATS — HMM is normal; the real HMM issue is the UVM
> device-private carve above); `GPU Fabric GUID: N/A` / `ClusterUUID=0` (normal for single-node
> FABRIC_MODE=0).

**Status:** Root cause CONFIRMED (2026-06-16) from direct on-host evidence + spec/kernel/source
cross-validation. NVSwitch BAR fix IMPLEMENTED, built, deployed to g208, and VALIDATED (cuInit=0).

**Branch:** `feature/pcie-root-port-passthrough` · edit tree `./ch-rootport` · build on `ssh
cloud-hypervisor` (`~/cloud-hypervisor`) · deploy to g208 `/var/lib/mmt/cloud-hypervisor-rootport`.

---

## 1. The problem (what the previous session left)

Goal: NVIDIA **open** kernel module `cuInit(0) == 0` for single-node 8×H100 NVLS inside
cloud-hypervisor, SEG0/vIOMMU-off, GPUs + NVSwitches behind PCIe root ports, guest fabric manager.

Two experiments bounded the problem:

| Exp | GPUs | NVSwitches | Fabric | cuInit | NVSwitch FW |
|-----|------|-----------|--------|--------|-------------|
| 1 | behind RP (bus 01–08) | **flat on bus 00** | Completed ×8, NV18 | **3** (NOT_INITIALIZED) | OK |
| 2 | behind RP (bus 01–08) | **behind RP (bus 09–0c)** | not up | **802** (SYSTEM_NOT_READY) | **FAILS: SXid 10008** |

Reading of the two data points:

- **cuInit 3 → 802 when NVSwitches got root ports** ⇒ OpenRM's cuInit-time topology check
  requires **every** NVIDIA fabric device (GPUs *and* NVSwitches) to have a parent PCIe root port.
  With NVSwitches flat it fails the topology check (rc=3). With NVSwitches behind RPs the topology
  check passes; cuInit now fails later at SYSTEM_NOT_READY because the fabric never comes up.
- The fabric never comes up because **NVSwitch firmware init fails behind the root port**
  (`SXid 10008, Fatal, Firmware initialization failure`; FM → `NV_WARN_NOTHING_TO_DO`).

So the entire remaining problem reduces to: **make NVSwitch firmware init succeed while the
NVSwitch is behind the CH root port.** Everything GPU-side already works.

A `set_guest_bdf()` VFIO patch (aligning the child's MSI/BDF/requester-ID with the exposed
secondary-bus BDF) was tried and **did not** fix it. That was the right thing to rule out — see §4.

---

## 2. Root cause

**The NVSwitch's sole BAR (BAR0) is 64-bit *non-prefetchable*. CH places every 64-bit BAR in the
above-4 GiB MMIO hole. A PCI-to-PCI bridge can only forward *non-prefetchable* memory *below
4 GiB*. So behind the CH root port the guest cannot route MMIO to NVSwitch BAR0, and the
nvidia-nvswitch firmware handshake — a poll loop over BAR0 — times out as SXid 10008.**

### Why each link holds

1. **Real hardware (host `lspci -vvv`, devices on vfio-pci):**
   - NVSwitch `10de:22a3` (class **0x0680**, "Bridge"): `Region 0: Memory at cc000000
     (64-bit, **non-prefetchable**) [size=32M]`. Single BAR. Uses **MSI** (Count=1), *not* MSI-X.
   - GPU `10de:2330` (class 0x0302): Region 0/2/4 = 16M / 128G / 32M, **all 64-bit
     *prefetchable***. Uses MSI-X.

2. **CH allocates by 64-bit-ness, ignoring prefetchability** —
   `pci/src/vfio.rs` `allocate_bars` (≈L688–795): `region_type` is chosen *only* from
   `is_64bit_bar`. A 64-bit BAR → `Memory64BitRegion` → `mmio64_allocator`. Prefetchability is
   recorded on the BAR but never influences placement. So the NVSwitch's 64-bit non-prefetchable
   BAR0 lands in the **mmio64** hole.

3. **mmio64 is far above 4 GiB** — `arch/src/x86_64/layout.rs`: the 32-bit device hole is
   `MEM_32BIT_DEVICES_START = 0xC000_0000`, size **640 MiB**; `RAM_64BIT_START = 0x1_0000_0000`
   (4 GiB); the mmio64 device hole sits *above all guest RAM* (≈ `0x3fff_xx00_0000` in the 800 GiB
   guest).

4. **A PCI-to-PCI bridge's non-prefetchable window is 32-bit only** — PCI-to-PCI Bridge
   Architecture Spec: Type-1 header has a non-prefetchable Memory Base/Limit (offset 0x20, A[31:20],
   **no upper-32 register**) and a *separate* Prefetchable Memory Base/Limit (0x24) **with** upper-32
   registers (0x28/0x2C). Linux enforces this in `drivers/pci/setup-bus.c
   pci_bridge_check_ranges()`: only the *prefetchable* window ever gets `IORESOURCE_MEM_64`; a
   non-prefetchable BAR behind a bridge must be placed below 4 GiB. (CH's bridge matches the spec:
   `configuration.rs` makes the 32-bit non-prefetch window (reg 8) and the 64-bit *prefetchable*
   window (regs 9/10/11, reg9=0x0001_0001) writable.)

5. **The guest tried to fix it and CH refused** — *dispositive evidence*, already on disk in
   `/var/lib/mmt/mmt-vm1-ch.log` on g208 (the Exp2 run):
   ```
   WARN pci/src/bus.rs:387 Failed moving device BAR: failed allocating new MMIO range:
        0x3fffd4000000->0xc0000000(0x2000000), keeping old BAR   # NVSwitch 0, 32 MiB
        0x3fffd2000000->0xc2000000(0x2000000), keeping old BAR   # NVSwitch 1
        0x3fffd0000000->0xc4000000(0x2000000), keeping old BAR   # NVSwitch 2
        0x3fffce000000->0xc6000000(0x2000000), keeping old BAR   # NVSwitch 3
   ```
   The guest kernel, seeing each NVSwitch behind a root port, correctly tries to relocate its
   non-prefetchable BAR from the high mmio64 address into the **below-4 GiB** 32-bit device hole
   (`0xC000_0000`+). CH **rejects every move**.

6. **Why CH rejects the move** — `vmm/src/device_manager.rs` `DeviceRelocation::move_bar`
   (≈L758–797) selects the allocator *list* by the BAR's `region_type`:
   `Memory64BitRegion → pci_mmio64_allocators` *only*. It then calls
   `allocate(new_base = 0xC000_0000)` on the mmio64 allocator, whose managed range starts far above
   4 GiB → returns `None` → `"failed allocating new MMIO range"` → `restore_bar_addr` writes the old
   high address back. Net effect: the guest writes `0xC000_0000` to BAR0, reads back
   `0x3fff_d400_0000`; a BAR that won't hold what was written is treated by Linux as
   unassignable → BAR0 left unassigned/unroutable behind the bridge.

7. **SXid 10008 is precisely a BAR0-MMIO poll failure** — open `nvidia-nvswitch` source
   (`ls10.c`, `nvswitch_check_io_sanity_ls10`, the HW_HOST 10000–10010 block, *not* the SOE 26000+
   block): the firmware-init check is a **pure poll loop over constant BAR0 offsets at probe time,
   before MMIO discovery, with no interrupt involved.** If BAR0 is unroutable the reads return
   `0xFFFFFFFF` and the poll times out → `SXid 10008`. This is why the MSI/requester-ID experiment
   was a dead end (§4) and why the failure is exactly "works flat, breaks behind a bridge."

### Why GPUs are unaffected
All three GPU BARs are **prefetchable** 64-bit → Linux places them in the bridge's **64-bit
prefetchable** window (which CH advertises and supports). No below-4 GiB constraint, no move needed.
Only the NVSwitch has a non-prefetchable 64-bit BAR, so only the NVSwitch breaks behind a bridge.
This GPU/NVSwitch asymmetry is the single strongest corroborator: whatever breaks must be something
the *root port* changes (a parent-window constraint) **and** must hit only the non-prefetchable-BAR
device. The BAR-window theory is the only candidate that fits both.

---

## 3. The fix

**Principle: decouple "which MMIO aperture a BAR lives in" from "the BAR is 64-bit-capable." A
64-bit BAR may legally hold a <4 GiB address (upper dword 0). Non-prefetchable memory behind a
bridge must live <4 GiB.**

### Fix A (primary, minimal, general) — `move_bar` selects the allocator by *address*, not by 64-bitness
In `vmm/src/device_manager.rs` `DeviceRelocation::move_bar`, for the
`Memory32BitRegion | Memory64BitRegion` arm: find the source allocator by which aperture owns
`old_base` (search **both** `pci_mmio32_allocators` and `pci_mmio64_allocators`), free there; find
the destination allocator by which aperture owns `new_base` (search both lists), allocate there;
restore on failure. This lets the guest's *correct* relocation of the NVSwitch BAR into the 32-bit
hole succeed. It is general (fixes any cross-aperture BAR move) and touches one function.

*Why this works here:* under `SEG0=1` there is a single segment 0, so one mmio32 allocator owns the
whole `0xC000_0000` 640 MiB hole — exactly the target the guest chose — and it is essentially empty
(GPU BARs are all 64-bit prefetchable in mmio64; the NVSwitch's only BAR is the one being moved). So
the four 32 MiB moves (128 MiB total) fit trivially.

### Fix B (additive robustness) — place non-prefetchable BARs of root-ported devices in mmio32 from the start
In `vfio.rs allocate_bars`, when the device will sit behind a root port, allocate **non-prefetchable**
memory BARs (32-bit *or* 64-bit) from the `mmio32_allocator` (<4 GiB) regardless of the 64-bit flag.
Then the guest finds BAR0 already at a forwardable address and need not move it at all — robust even
for guests that don't reassign. Requires threading the "behind RP" decision (currently made in
`device_manager` `add_pci_device`, the `needs_nvidia_root_port` predicate) into BAR allocation, or
conservatively applying it to any device whose config-probed class is NVSwitch (0x0680 / id
`nvsw-*`). Scope it to root-ported devices so flat devices (which work high today) are unchanged and
the 640 MiB hole isn't pressured.

**Recommended:** implement **A** (it directly enables observed guest behavior and is the smaller,
more general change); add **B** if any guest is found that does not reassign. Re-enable the NVSwitch
root-port predicate that the previous session reverted (include NVSwitch class 0x0680 / `nvsw-*`
alongside the GPU `gpu-*` / class 0x03 path in `needs_nvidia_root_port`).

### ⚠ Load-bearing correctness requirement (must ship with EITHER fix)
Two other code paths *also* select the MMIO allocator pool by the BAR's 64-bit flag, not by address.
If a 64-bit BAR ends up living in the mmio32 hole (whether via Fix A's guest move or Fix B's initial
placement), these will silently desync/leak the allocator:
- `vmm/src/device_manager.rs` `move_bar` (L758–763): picks `pci_mmio64_allocators` for any
  `Memory64BitRegion` BAR. **(This is the bug for Fix A; fix it to be address-based.)**
- `pci/src/vfio.rs` `free_bars` (L846–850): frees a `Memory64BitRegion` region from
  `mmio64_allocator`. After a 64-bit BAR has been parked/moved into mmio32, teardown would call
  `mmio64_allocator.free(<mmio32 addr>)` → wrong-pool free → leak/desync. The relocation update at
  vfio.rs ~L1997 already moves `MmioRegion.start` to the new (mmio32) address, so `free_bars` *will*
  see the mmio32 address with a `Memory64BitRegion` type — exactly the desync.

**Cleanest robust form:** select the allocator pool by *which allocator's `[base,end]` contains the
address* in both `move_bar` (use `new_base` for the destination, `old_base` for the source) and
`free_bars` (use `region.start`), searching both `pci_mmio32_allocators` and `pci_mmio64_allocators`.
Alternatively record the originating pool on `MmioRegion` and consult it. Do **not** rely on
`region_type` for pool selection anywhere once 64-bit BARs can live low.

### Ordering dependency (only relevant to Fix B)
The root-port decision (`device_manager.rs` `needs_nvidia_root_port`, ≈L4045) currently runs *after*
`allocate_bars` (via `pci_resources`, ≈L4017). Fix B (place low at allocation time) needs the
`behind_root_port` flag *before* BAR allocation, so the decision must be pre-computed/reordered, or
applied unconditionally to NVSwitch-class non-prefetchable BARs. **Fix A avoids this entirely** — the
guest drives the move, so no allocation-time flag is needed. That's the main reason A is recommended
as the primary change.

### Other edge cases
- 64-bit BAR holding a <4 GiB address: legal and standard (upper dword 0); keep the BAR's 64-bit
  type bit set in config space, only the *address* changes aperture.
- 640 MiB hole capacity under non-SEG0 multi-segment configs: the 32-bit hole is split per segment by
  aperture weight; if NVSwitches are ever placed on their own segment (launcher default puts them on
  segment 9 when not SEG0), that segment's mmio32 slice must be ≥ 4×32 MiB. Under the working
  `SEG0=1` recipe this is a non-issue (single segment owns all 640 MiB). `allocate_bars` already
  fails loud (`IoAllocationFailed`) on exhaustion — no silent fallback.
- Broaden the root-port gate (`device_manager.rs` ≈L4060) to recognize the NVSwitch (vendor 0x10de,
  **class 0x06**, device 0x22a3 / launcher id `nvsw-*`); today it only matches GPU class 0x03 /
  `gpu-*`. Ensure it does not accidentally route non-NVIDIA bridges behind a root port.

---

## 4. Ruled out (and why)
- **MSI / requester-ID / `set_guest_bdf`** — already tried, didn't help; NVSwitch uses MSI(1) not
  MSI-X; and SXid 10008 is a *poll loop*, not interrupt-gated (§2.7). The handoff's `set_guest_bdf`
  revert was correct.
- **"CH's flat MMIO bus makes BAR0 reachable anyway, so routing can't be the cause"** — the
  adversarial review's strongest objection. Refuted: the failure is *upstream* of any MMIO access.
  The guest kernel won't place a non-prefetchable BAR outside a parent bridge window, and the
  `ch.log` proves the guest tried to move BAR0 below 4 GiB and CH blocked it (`restore_bar_addr`
  forced the high address back), so the kernel leaves BAR0 unassigned/MSE-clear and the driver never
  ioremaps the high GPA. Emulator-level reachability ≠ guest assignability.
- **Missing secondary-bus-reset (CH's Bridge Control SBR bit is a no-op)** — SBR is a no-op in
  *both* flat and behind-RP configs, so it cannot explain "works flat, breaks behind RP."
- **Missing AER/ACS/DSN/slot/hotplug on the root port** — does not block firmware init; GPUs init
  fine behind the same minimal root port.
- **Extended-config visibility through the secondary bus** — `bus.rs` routes full 4 KiB ECAM
  (register mask 0x3ff) to the child unconditionally; GPUs' extended caps work, so this is fine.
- **False AtomicOp completer advertisement on the root port** (`root_port.rs` DevCap2) — possible
  contributor at most; the discriminating test is in §5 but it does not fit the asymmetry as cleanly.

---

## 5. Verification plan (respect hardware safety)

**Safety (unchanged):** never run NCCL/NVLS unless **(1)** `nvidia-smi -q` shows Fabric State
Completed ×8 **and (2)** `cuInit(0) == 0`. Clean GPU test needs a host reboot with the NVIDIA driver
blocked, then manual `vfio-pci` bind of 8 GPUs + 4 NVSwitches. Never brick a remote node without
verified console/power access.

**Pre-fix confirmation (optional, no NCCL, no reboot — host already in vfio state):** boot the
NVSwitch-behind-RP binary (`PASS_NVSWITCH=1`), then in the guest collect the BEFORE evidence:
- `lspci -vvv -s <nvswitch>` Region 0 → expect base ≥ `0x1_0000_0000` `(64-bit, non-prefetchable)`
  marked `[disabled]`/unassigned/addr 0, and `Control: ... Mem-` (MSE clear). MSE-set + high address
  **refutes** the "can't reach BAR0" mechanism — look elsewhere.
- `lspci -vvv -s <rootport>` → "Memory behind bridge" (non-prefetchable) empty/`<none>` or not
  covering BAR0; "Prefetchable memory behind bridge" covers the large GPU ranges.
- `dmesg | grep -iE 'pci 0000:0[9abcd].*(BAR 0|no space|failed to assign|can.?t claim)|bridge window'`
  → expect `BAR 0: no space for [mem size 0x2000000 64bit]` / `failed to assign`, and on the bridge
  `can't handle bridge window above 4GB` / `disabling bridge window`.
- `setpci -s <nvswitch> COMMAND` → expect MSE (bit 1) = 0 (vs a GPU BDF where MSE=1 in both states).
- (host-side proof, already captured) `/var/lib/mmt/mmt-vm1-ch.log` shows the 4 rejected BAR moves
  `0x3fff…->0xc000_0000(0x2000000) keeping old BAR`.

**Post-fix (the decisive test):** implement Fix A (+ the `free_bars` consistency fix), rebuild on
`ssh cloud-hypervisor`, deploy to g208, boot the standard recipe + `PASS_NVSWITCH=1` with NVSwitches
root-ported. Then:
- guest `lspci -vvv -s <nvswitch>` → Region 0 assigned at a **<4 GiB** address inside the bridge's
  non-prefetchable window, `Mem+`; `lspci -vvv -s <rootport>` "Memory behind bridge" now covers it.
- `dmesg | grep -i 'SXid\|10008'` → empty; nvidia-nvswitch reports init success.
- `sudo systemctl restart nvidia-fabricmanager; nvidia-smi -q | grep -c Completed` → 8.
- `python3 -c 'import ctypes; print(ctypes.CDLL("libcuda.so.1").cuInit(0))'` → **0**.
- Only if Fabric Completed ×8 **and** cuInit==0: run the NVLS/NCCL benchmark.

**Direct BAR0 probe (tightest signal):** after binding nvidia-nvswitch, mmap `resource0` and read
offset `0x660bc` (`NV_GFW_GLOBAL_BOOT_PARTITION_PROGRESS`). BEFORE: `0xFFFFFFFF` (BAR0 unreachable);
AFTER: the VALUE_SUCCESS field becomes `0xFF` once firmware boots. This is the exact register whose
poll-timeout produces SXid 10008.

**Negative controls (lock the conclusion):** (a) add AER+ACS+SSVID to the root port *without* the
BAR-placement fix → predict SXid 10008 persists. (b) zero the root port's DevCap2 AtomicOp bits
(`root_port.rs:76`) *without* the fix → predict 10008 persists. Both confirm the BAR window, not
caps/atomics, is the cause. If SXid 10008 persists *after* Fix A with BAR0 now <4 GiB, readable, and
MSE=1, the addressing hypothesis is refuted for the residual — escalate to §4 secondary candidates.

---

## 6. Standard launch recipe (g208)
```
sudo env CH_ROOT_PORT_PASSTHROUGH=1 SEG0=1 MEM_GB_TOTAL=800 PASS_NVSWITCH=1 \
  VMNAME=mmt-vm1 GUEST_IP=192.168.201.2 HOST_TAP_IP=192.168.201.1 NODE_PRIVATE_IP=10.15.26.1 \
  CH_BIN=/var/lib/mmt/cloud-hypervisor-rootport KERNEL=/var/lib/mmt/guest-vmlinuz \
  INITRD=/var/lib/mmt/guest-initrd RAW_ROOT=1 REUSE_DISKS=1 TAP=mmt-tap0 UPLINK=enp27s0f0np0 \
  bash /var/lib/mmt/mmt-vm.sh up
```
Device map (host): GPUs `19/3b/4c/5d/9b/bb/cb/db:00.0`, NVSwitches `83/84/85/86:00.0`, paired
ConnectX-7 IB on the odd BDFs. Launcher `/var/lib/mmt/mmt-vm.sh`; `SEG0=1` forces all devices onto
segment 0 (`num_pci_segments=1`, no vIOMMU). Guest: `ssh -i /var/lib/mmt/vmkey ubuntu@192.168.201.2`.
