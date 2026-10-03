//! Checks the NVMe character-device deployment constraint: when a container is configured for the NVMe path but the device node or required capability is absent, startup refuses with a diagnostic rather than silently falling back to the filesystem path.

/// conformance: hef-hardware-deployment/nvme-character-device-deployment-constraint/container-missing-device-node-or-capability
#[test]
fn container_missing_device_node_or_capability() {
    // stub: blocked on the NVMe deployment-config and refusing startup probe path (D6); lands with the NVMe deployment-configuration and device-presence-check changes.
}
