// Copyright © 2026 Lightning AI
//
// SPDX-License-Identifier: Apache-2.0
//
// A PCIe Root Port: a type-1 PCI-to-PCI bridge device that presents a single
// downstream (secondary) bus carrying one passed-through endpoint (a GPU).
//
// Why this exists: NVIDIA's open kernel module (OpenRM) refuses to initialize
// CUDA (cuInit returns CUDA_ERROR_NOT_INITIALIZED / rc=3) when a passed-through
// GPU sits directly on a flat PCI root bus, as cloud-hypervisor presents it
// today (one host bridge per segment, endpoints on bus 0). OpenRM walks the
// PCIe parent chain during init and bails when no root port is found above the
// GPU. QEMU's pcie-root-port satisfies this; cloud-hypervisor has no root-port
// device model. This device fills that gap. See the multi-bus config routing in
// bus.rs which dispatches config cycles for the secondary bus to the child.

use std::any::Any;
use std::sync::{Arc, Barrier};

use crate::configuration::{
    PciBridgeSubclass, PciCapability, PciCapabilityId, PciClassCode, PciConfiguration,
    PciHeaderType, PciSubclass,
};
use crate::device::{BarReprogrammingParams, PciDevice};

// Bus number register lives at config offset 0x18 (dword index 6):
// [7:0] primary, [15:8] secondary, [23:16] subordinate, [31:24] sec latency.
const BUS_NUMBER_REG: usize = 6;

// Type-1 bridge BAR registers (config offsets 0x10 and 0x14). A PCIe root port
// has no BARs, so these read as 0 and ignore writes. Without intercepting them,
// the guest's BAR-sizing writes (0xffffffff probes) reach PciConfiguration's
// detect_bar_reprogramming, which enters its 64-bit-BAR branch for an
// unconfigured BAR and panics on `decode_64_bits_bar_size(...).unwrap()`
// (size is 0). Intercepting here keeps those writes away from that path.
const BAR0_REG: usize = 4;
const BAR1_REG: usize = 5;

// Present as a QEMU-compatible PCIe Root Port so guest enumeration and OpenRM
// recognize a well-known root-port identity.
const ROOT_PORT_VENDOR_ID: u16 = 0x1b36; // Red Hat, Inc.
const ROOT_PORT_DEVICE_ID: u16 = 0x000c; // QEMU PCIe Root Port

/// PCI Express capability advertising this bridge as a Root Port (Device/Port
/// Type = 0x4) with a trained gen-class x16 link and AtomicOp completer support.
///
/// `bytes()` returns the capability content *after* the 2-byte capability header
/// (`add_capability` writes the cap-id and next-pointer itself).
struct PciExpressRootPortCap {
    data: Vec<u8>,
}

impl PciExpressRootPortCap {
    fn new() -> Self {
        // v2 PCI Express capability structure is 0x3C bytes total; minus the
        // 2-byte header => 0x3A bytes of content.
        let mut data = vec![0u8; 0x3a];

        // PCI Express Capabilities Register (cap offset 0x02 -> index 0x00):
        // version 2, Device/Port Type = Root Port (0x4), Slot Implemented.
        let caps: u16 = 0x0002 | (0x4 << 4) | (1 << 8); // 0x0142
        data[0x00..0x02].copy_from_slice(&caps.to_le_bytes());

        // Link Capabilities (cap offset 0x0C -> index 0x0A): max width x16,
        // max speed 16 GT/s (encoding 4).
        let link_cap: u32 = 0x4 | (0x10 << 4); // 0x104
        data[0x0a..0x0e].copy_from_slice(&link_cap.to_le_bytes());

        // Link Status (cap offset 0x12 -> index 0x10): negotiated x16 / 16 GT/s.
        let link_sta: u16 = 0x4 | (0x10 << 4); // 0x104
        data[0x10..0x12].copy_from_slice(&link_sta.to_le_bytes());

        // Device Capabilities 2 (cap offset 0x24 -> index 0x22): AtomicOp
        // routing supported + 32-bit and 64-bit AtomicOp completer supported.
        // OpenRM inspects these when validating the GPU's parent root port.
        let devcap2: u32 = (1 << 6) | (1 << 7) | (1 << 8); // 0x1c0
        data[0x22..0x26].copy_from_slice(&devcap2.to_le_bytes());

        Self { data }
    }
}

impl PciCapability for PciExpressRootPortCap {
    fn bytes(&self) -> &[u8] {
        &self.data
    }

    fn id(&self) -> PciCapabilityId {
        PciCapabilityId::PciExpress
    }
}

/// A PCIe root port bridge. Bus numbers are fixed at construction (cloud-
/// hypervisor assigns the topology; the guest is booted without
/// `pci=assign-busses` for this mode) so config-cycle routing is deterministic.
pub struct PciRootPort {
    id: String,
    config: PciConfiguration,
    secondary_bus: u8,
    // Cached value of the bus-number register (offset 0x18). Kept here rather
    // than in PciConfiguration because the type-1 bridge header does not mark
    // this register writable, and the value is fixed for this mode.
    bus_number_reg: u32,
}

impl PciRootPort {
    /// Build a root port whose single child endpoint lives on `secondary_bus`.
    pub fn new(id: String, secondary_bus: u8) -> Self {
        let mut config = PciConfiguration::new(
            ROOT_PORT_VENDOR_ID,
            ROOT_PORT_DEVICE_ID,
            0x01,
            PciClassCode::BridgeDevice,
            &PciBridgeSubclass::PciToPciBridge as &dyn PciSubclass,
            None,
            PciHeaderType::Bridge,
            0,
            0,
            None,
            None,
        );

        // PCIe root ports are single-bus bridges here: secondary == subordinate.
        let bus_number_reg: u32 =
            (u32::from(secondary_bus) << 8) | (u32::from(secondary_bus) << 16);

        // The PCI Express capability is what makes OpenRM accept this as a root
        // port; failure to add it would leave the bridge looking like a plain
        // legacy PCI-PCI bridge, so surface it loudly during bring-up.
        config
            .add_capability(&PciExpressRootPortCap::new())
            .expect("PCIe root-port capability must fit in bridge config space");

        PciRootPort {
            id,
            config,
            secondary_bus,
            bus_number_reg,
        }
    }

    /// The downstream bus number this root port forwards config cycles to.
    pub fn secondary_bus(&self) -> u8 {
        self.secondary_bus
    }
}

impl PciDevice for PciRootPort {
    fn write_config_register(
        &mut self,
        reg_idx: usize,
        offset: u64,
        data: &[u8],
    ) -> (Vec<BarReprogrammingParams>, Option<Arc<Barrier>>) {
        // Bus numbers are fixed in this mode; ignore guest attempts to renumber.
        // BAR registers: a root port has no BARs, so ignore the guest's sizing
        // writes (they would otherwise trip a panic in detect_bar_reprogramming).
        if reg_idx == BUS_NUMBER_REG || reg_idx == BAR0_REG || reg_idx == BAR1_REG {
            return (Vec::new(), None);
        }
        (
            self.config.write_config_register(reg_idx, offset, data),
            None,
        )
    }

    fn read_config_register(&mut self, reg_idx: usize) -> u32 {
        if reg_idx == BUS_NUMBER_REG {
            return self.bus_number_reg;
        }
        // Root port has no BARs: report empty BAR registers.
        if reg_idx == BAR0_REG || reg_idx == BAR1_REG {
            return 0;
        }
        self.config.read_config_register(reg_idx)
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn id(&self) -> Option<String> {
        Some(self.id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_port_identity_and_topology() {
        let mut rp = PciRootPort::new("rp0".to_string(), 1);

        // Vendor/device id (register 0).
        let reg0 = rp.read_config_register(0);
        assert_eq!(reg0 & 0xffff, u32::from(ROOT_PORT_VENDOR_ID));
        assert_eq!(reg0 >> 16, u32::from(ROOT_PORT_DEVICE_ID));

        // Header type 1 (bridge) in register 3, byte 2.
        let reg3 = rp.read_config_register(3);
        assert_eq!((reg3 >> 16) & 0xff, 0x01);

        // Class code: bridge (0x06), subclass PCI-to-PCI (0x04) in register 2.
        let reg2 = rp.read_config_register(2);
        assert_eq!(reg2 >> 24, 0x06);
        assert_eq!((reg2 >> 16) & 0xff, 0x04);

        // Bus numbers: secondary == subordinate == 1, primary 0.
        let busreg = rp.read_config_register(BUS_NUMBER_REG);
        assert_eq!(busreg & 0xff, 0); // primary
        assert_eq!((busreg >> 8) & 0xff, 1); // secondary
        assert_eq!((busreg >> 16) & 0xff, 1); // subordinate
        assert_eq!(rp.secondary_bus(), 1);

        // Guest writes to the bus-number register are ignored (fixed topology).
        rp.write_config_register(BUS_NUMBER_REG, 0, &0x00ff_ff00u32.to_le_bytes());
        assert_eq!(rp.read_config_register(BUS_NUMBER_REG), busreg);
    }
}
