//! Hostname and hardware detection, independent of whether a daemon serves or receives.
use gmux_protocol::model::MachineKind;
use std::{
    fs::File,
    io::{BufRead, BufReader, Read},
    path::Path,
    sync::OnceLock,
};

pub fn hostname() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        let mut bytes = [0u8; 256];
        // The buffer is writable and includes room for Linux's hostname limit.
        if unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
            return "localhost".into();
        }
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        let name = String::from_utf8_lossy(&bytes[..end]).trim().to_owned();
        if name.is_empty() {
            "localhost".into()
        } else {
            name
        }
    })
}

pub fn machine_kind() -> MachineKind {
    static KIND: OnceLock<MachineKind> = OnceLock::new();
    *KIND.get_or_init(|| detect(Path::new("/")))
}

fn chassis(value: &str) -> Option<MachineKind> {
    Some(match value.trim().trim_end_matches('\0') {
        "desktop" => MachineKind::Desktop,
        "laptop" => MachineKind::Laptop,
        "convertible" => MachineKind::Convertible,
        "server" => MachineKind::Server,
        "tablet" => MachineKind::Tablet,
        "handset" => MachineKind::Phone,
        "watch" => MachineKind::Watch,
        "vm" => MachineKind::VirtualMachine,
        "container" => MachineKind::Container,
        "embedded" => MachineKind::Embedded,
        _ => return None,
    })
}

fn dmi(value: &str) -> Option<MachineKind> {
    // SMBIOS system enclosure types, including rack servers and convertibles.
    Some(match value.trim().parse::<u8>().ok()? {
        3 | 4 | 6 | 7 | 13 | 35 | 36 => MachineKind::Desktop,
        8 | 9 | 10 | 14 => MachineKind::Laptop,
        17 | 23 | 28 | 29 => MachineKind::Server,
        30 => MachineKind::Tablet,
        11 => MachineKind::Phone,
        31 | 32 => MachineKind::Convertible,
        33 | 34 => MachineKind::Embedded,
        _ => return None,
    })
}

fn acpi(value: &str) -> Option<MachineKind> {
    Some(match value.trim().parse::<u8>().ok()? {
        1 | 3 | 6 => MachineKind::Desktop,
        2 => MachineKind::Laptop,
        4 | 5 | 7 => MachineKind::Server,
        8 => MachineKind::Tablet,
        _ => return None,
    })
}

fn detect(root: &Path) -> MachineKind {
    let read = |path: &str| std::fs::read_to_string(root.join(path)).ok();
    read("etc/machine-info")
        .and_then(|info| {
            info.lines()
                .rev()
                .find_map(|line| line.trim().strip_prefix("CHASSIS="))
                .and_then(|value| chassis(value.trim().trim_matches(['\'', '"'])))
        })
        .or_else(|| virtual_kind(root))
        .or_else(|| read("sys/class/dmi/id/chassis_type").and_then(|value| dmi(&value)))
        .or_else(|| read("sys/firmware/acpi/pm_profile").and_then(|value| acpi(&value)))
        .or_else(|| {
            read("sys/firmware/devicetree/base/chassis-type").and_then(|value| chassis(&value))
        })
        .unwrap_or_default()
}

fn virtual_kind(root: &Path) -> Option<MachineKind> {
    let read = |path: &str| std::fs::read_to_string(root.join(path)).ok();
    // Container identity takes precedence over the host's exposed firmware.
    if ["run/.containerenv", ".dockerenv", ".flatpak-info"]
        .iter()
        .any(|path| root.join(path).is_file())
        || ["run/systemd/container", "run/host/container-manager"]
            .iter()
            .any(|path| read(path).is_some_and(|name| !name.trim().is_empty()))
        || (root.join("proc/vz").exists() && !root.join("proc/bc").exists())
        || std::fs::read(root.join("proc/1/environ")).is_ok_and(|env| {
            env.split(|byte| *byte == 0).any(|entry| {
                entry
                    .strip_prefix(b"container=")
                    .is_some_and(|value| !value.is_empty())
            })
        })
    {
        return Some(MachineKind::Container);
    }
    // WSL exposes its environment through the kernel release, without DMI.
    if read("proc/sys/kernel/osrelease")
        .is_some_and(|release| release.contains("Microsoft") || release.contains("microsoft"))
    {
        return Some(MachineKind::VirtualMachine);
    }
    let product = read("sys/class/dmi/id/product_name");
    let vendor = read("sys/class/dmi/id/sys_vendor");
    if vendor
        .as_deref()
        .is_some_and(|name| name.starts_with("Amazon EC2"))
    {
        return Some(
            if product
                .as_deref()
                .and_then(|name| name.trim().split_once(".metal"))
                .is_some_and(|(_, suffix)| suffix.is_empty() || suffix.starts_with('-'))
            {
                MachineKind::Server
            } else {
                MachineKind::VirtualMachine
            },
        );
    }
    // Match virtual product identities, not ordinary hardware vendors such as
    // Microsoft or Oracle, which also manufacture physical computers.
    let virtual_name = |name: &str| {
        [
            "KVM",
            "QEMU",
            "VMware",
            "VMW",
            "innotek GmbH",
            "VirtualBox",
            "Xen",
            "Bochs",
            "Parallels",
            "BHYVE",
            "bhyve",
            "Hyper-V",
            "Virtual Machine",
            "OpenStack",
            "KubeVirt",
            "Alibaba Cloud ECS",
            "Apple Virtualization",
        ]
        .iter()
        .any(|prefix| name.trim().starts_with(prefix))
    };
    if product.as_deref().is_some_and(virtual_name)
        || vendor.as_deref().is_some_and(virtual_name)
        || [
            "sys/class/dmi/id/board_vendor",
            "sys/class/dmi/id/bios_vendor",
            "sys/class/dmi/id/product_version",
        ]
        .iter()
        .any(|path| read(path).is_some_and(|name| virtual_name(&name)))
    {
        return Some(MachineKind::VirtualMachine);
    }

    // Xen's hardware domain is the host, even though it exposes a hypervisor.
    let xen_host = read("proc/xen/capabilities")
        .is_some_and(|caps| caps.split_whitespace().any(|value| value == "control_d"));
    let cpu_virtual = cpu_virtual(root);
    if !xen_host
        && (read("sys/hypervisor/type").is_some_and(|kind| !kind.trim().is_empty())
            || cpu_virtual == Some(true)
            || smbios_virtual(root)
            || read("sys/firmware/devicetree/base/hypervisor/compatible")
                .is_some_and(|kind| !kind.trim_matches(['\0', ' ', '\n']).is_empty())
            || read("sys/firmware/devicetree/base/compatible").is_some_and(|compat| {
                compat
                    .split('\0')
                    .any(|value| matches!(value, "linux,dummy-virt" | "qemu,pseries"))
            }))
    {
        return Some(MachineKind::VirtualMachine);
    }
    // GCE uses the same vendor identity on VMs and bare-metal servers. On x86,
    // the kernel's CPU flags distinguish them; on ARM these flags may be absent.
    if [product.as_deref(), vendor.as_deref()]
        .into_iter()
        .flatten()
        .any(|name| name.starts_with("Google Compute Engine"))
    {
        return Some(
            if cfg!(target_arch = "x86_64") && cpu_virtual == Some(false) {
                MachineKind::Server
            } else {
                MachineKind::VirtualMachine
            },
        );
    }
    None
}

fn cpu_virtual(root: &Path) -> Option<bool> {
    let mut cpu = BufReader::new(File::open(root.join("proc/cpuinfo")).ok()?);
    let mut line = String::new();
    // Stop at the first CPU's flags, avoiding a full read on large servers.
    while cpu.read_line(&mut line).ok()? != 0 {
        if let Some((field, value)) = line.split_once(':') {
            if field.trim() == "vendor_id" && value.trim() == "User Mode Linux" {
                return Some(true);
            }
            if matches!(field.trim(), "flags" | "Features") {
                return Some(value.split_whitespace().any(|flag| flag == "hypervisor"));
            }
        }
        line.clear();
    }
    None
}

fn smbios_virtual(root: &Path) -> bool {
    let mut bios = [0u8; 20];
    // SMBIOS BIOS Characteristics Extension Byte 2, bit 4 means a VM. This
    // table can be restricted to root, so unavailable data is inconclusive.
    File::open(root.join("sys/firmware/dmi/entries/0-0/raw"))
        .and_then(|mut file| file.read_exact(&mut bios))
        .is_ok()
        && bios[1] >= 20
        && bios[19] & (1 << 4) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Root(std::path::PathBuf);

    impl Root {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "gmux-machine-{}",
                crate::unix::identity::daemon_id().unwrap()
            )))
        }

        fn write(&self, path: &str, value: impl AsRef<[u8]>) {
            let path = self.0.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, value).unwrap();
        }

        fn detect(&self) -> MachineKind {
            detect(&self.0)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            if self.0.exists() {
                std::fs::remove_dir_all(&self.0).unwrap();
            }
        }
    }

    #[test]
    fn firmware_types_distinguish_physical_chassis_without_guessing() {
        for (values, expected) in [
            (vec![3, 4, 6, 7, 13, 35, 36], MachineKind::Desktop),
            (vec![8, 9, 10, 14], MachineKind::Laptop),
            (vec![17, 23, 28, 29], MachineKind::Server),
            (vec![30], MachineKind::Tablet),
            (vec![11], MachineKind::Phone),
            (vec![31, 32], MachineKind::Convertible),
            (vec![33, 34], MachineKind::Embedded),
        ] {
            for value in values {
                assert_eq!(dmi(&value.to_string()), Some(expected));
            }
        }
        for value in ["", "unknown", "0", "1", "2", "255", "300"] {
            assert_eq!(dmi(value), None);
        }
        for (values, expected) in [
            (vec![1, 3, 6], MachineKind::Desktop),
            (vec![2], MachineKind::Laptop),
            (vec![4, 5, 7], MachineKind::Server),
            (vec![8], MachineKind::Tablet),
        ] {
            for value in values {
                assert_eq!(acpi(&value.to_string()), Some(expected));
            }
        }
    }

    #[test]
    fn virtual_identity_overrides_emulated_firmware_but_not_physical_hosts() {
        for (path, value) in [
            ("sys/class/dmi/id/sys_vendor", "QEMU"),
            ("sys/class/dmi/id/product_name", "KVM Virtual Machine"),
            ("sys/class/dmi/id/product_name", "VMware Virtual Platform"),
            ("sys/class/dmi/id/bios_vendor", "VMW"),
            ("sys/class/dmi/id/sys_vendor", "innotek GmbH"),
            ("sys/class/dmi/id/product_name", "VirtualBox"),
            ("sys/class/dmi/id/sys_vendor", "OpenStack Foundation"),
            ("sys/class/dmi/id/product_name", "KubeVirt"),
            ("sys/class/dmi/id/product_name", "Alibaba Cloud ECS"),
            ("sys/class/dmi/id/sys_vendor", "Bochs"),
            ("sys/class/dmi/id/product_version", "Hyper-V UEFI Release"),
            ("sys/class/dmi/id/product_name", "Virtual Machine"),
            (
                "sys/class/dmi/id/sys_vendor",
                "Parallels Software International Inc.",
            ),
            ("sys/class/dmi/id/product_name", "BHYVE"),
            ("sys/class/dmi/id/product_name", "Apple Virtualization"),
            ("sys/hypervisor/type", "xen"),
            ("proc/cpuinfo", "flags\t: fpu hypervisor sse\n"),
            ("proc/cpuinfo", "vendor_id\t: User Mode Linux\n"),
            (
                "proc/sys/kernel/osrelease",
                "6.6.87.2-microsoft-standard-WSL2",
            ),
            (
                "sys/firmware/devicetree/base/hypervisor/compatible",
                "linux,kvm\0",
            ),
            (
                "sys/firmware/devicetree/base/compatible",
                "other,board\0linux,dummy-virt\0",
            ),
            ("sys/firmware/devicetree/base/compatible", "qemu,pseries\0"),
        ] {
            let root = Root::new();
            root.write("sys/class/dmi/id/chassis_type", "3\n");
            root.write(path, value);
            assert_eq!(root.detect(), MachineKind::VirtualMachine, "{path}={value}");
        }

        let root = Root::new();
        // The reported VPS exposes an unknown chassis and desktop ACPI profile.
        root.write("sys/class/dmi/id/sys_vendor", "QEMU\n");
        root.write("sys/class/dmi/id/product_name", "KVM Virtual Machine\n");
        root.write("sys/class/dmi/id/chassis_type", "1\n");
        root.write("sys/firmware/acpi/pm_profile", "1\n");
        assert_eq!(root.detect(), MachineKind::VirtualMachine);
        root.write("sys/class/dmi/id/sys_vendor", "Amazon EC2\n");
        for (product, expected) in [
            ("c7i.metal-24xl\n", MachineKind::Server),
            ("i3.metal\n", MachineKind::Server),
            ("c7i.4xlarge\n", MachineKind::VirtualMachine),
            ("unknown.metallic\n", MachineKind::VirtualMachine),
        ] {
            root.write("sys/class/dmi/id/product_name", product);
            assert_eq!(root.detect(), expected, "{product}");
        }

        for path in [
            "sys/class/dmi/id/product_name",
            "sys/class/dmi/id/sys_vendor",
        ] {
            let root = Root::new();
            root.write(path, "Google Compute Engine\n");
            assert_eq!(root.detect(), MachineKind::VirtualMachine);
            root.write("proc/cpuinfo", "flags: fpu hypervisor\n");
            assert_eq!(root.detect(), MachineKind::VirtualMachine);
            if cfg!(target_arch = "x86_64") {
                root.write("proc/cpuinfo", "flags: fpu\n");
                assert_eq!(root.detect(), MachineKind::Server);
            }
        }
        let root = Root::new();
        root.write("sys/class/dmi/id/chassis_type", "17\n");
        for vendor in [
            "Microsoft Corporation",
            "Oracle Corporation",
            "Alibaba Cloud",
            "Dell Inc.",
        ] {
            root.write("sys/class/dmi/id/sys_vendor", vendor);
            assert_eq!(root.detect(), MachineKind::Server, "{vendor}");
        }
        root.write("sys/hypervisor/type", "xen\n");
        root.write("proc/xen/capabilities", "control_d\n");
        root.write("proc/cpuinfo", "flags: fpu hypervisor\n");
        assert_eq!(root.detect(), MachineKind::Server);
    }

    #[test]
    fn containers_take_precedence_over_exposed_host_or_vm_hardware() {
        for (path, value) in [
            (".dockerenv", ""),
            ("run/.containerenv", ""),
            (".flatpak-info", "[Application]\n"),
            ("run/systemd/container", "lxc\n"),
            ("run/systemd/container", "systemd-nspawn\n"),
            ("run/host/container-manager", "podman\n"),
            ("proc/1/environ", "LANG=C\0container=lxc-libvirt\0"),
            ("proc/vz", ""),
        ] {
            let root = Root::new();
            root.write("sys/class/dmi/id/product_name", "KVM Virtual Machine\n");
            root.write(path, value);
            assert_eq!(root.detect(), MachineKind::Container, "{path}");
        }
        let root = Root::new();
        root.write("sys/class/dmi/id/chassis_type", "23\n");
        root.write("proc/vz", "");
        root.write("proc/bc", "");
        root.write("run/systemd/container", "\n");
        root.write("run/host/container-manager", "");
        root.write("proc/1/environ", "container=\0CONTAINER=1\0");
        assert_eq!(root.detect(), MachineKind::Server);
    }

    #[test]
    fn generic_smbios_vm_flag_requires_a_complete_bios_structure() {
        let root = Root::new();
        root.write("sys/class/dmi/id/chassis_type", "3\n");
        let mut bios = [0u8; 20];
        bios[1] = 20;
        bios[19] = 1 << 4;
        root.write("sys/firmware/dmi/entries/0-0/raw", bios);
        assert_eq!(root.detect(), MachineKind::VirtualMachine);
        root.write("sys/firmware/dmi/entries/0-0/raw", &bios[..19]);
        assert_eq!(root.detect(), MachineKind::Desktop);
        bios[1] = 19;
        root.write("sys/firmware/dmi/entries/0-0/raw", bios);
        assert_eq!(root.detect(), MachineKind::Desktop);
    }

    #[test]
    fn detection_respects_machine_info_and_recovers_from_unavailable_firmware() {
        let root = Root::new();
        assert_eq!(root.detect(), MachineKind::Unknown);
        root.write("sys/firmware/devicetree/base/chassis-type", "tablet\0");
        assert_eq!(root.detect(), MachineKind::Tablet);
        root.write("sys/firmware/acpi/pm_profile", "4\n");
        assert_eq!(root.detect(), MachineKind::Server);
        root.write("sys/class/dmi/id/chassis_type", "10\n");
        assert_eq!(root.detect(), MachineKind::Laptop);
        root.write(".dockerenv", "");
        for (value, expected) in [
            ("desktop", MachineKind::Desktop),
            ("laptop", MachineKind::Laptop),
            ("convertible", MachineKind::Convertible),
            ("server", MachineKind::Server),
            ("tablet", MachineKind::Tablet),
            ("handset", MachineKind::Phone),
            ("watch", MachineKind::Watch),
            ("embedded", MachineKind::Embedded),
            ("vm", MachineKind::VirtualMachine),
            ("container", MachineKind::Container),
        ] {
            root.write(
                "etc/machine-info",
                format!("# CHASSIS=unknown\nCHASSIS=laptop\nCHASSIS=\"{value}\"\n"),
            );
            assert_eq!(root.detect(), expected, "{value}");
        }
        std::fs::remove_file(root.0.join(".dockerenv")).unwrap();
        root.write("etc/machine-info", "CHASSIS=invalid\n");
        root.write("sys/class/dmi/id/chassis_type", "invalid\n");
        assert_eq!(root.detect(), MachineKind::Server);
    }
}
