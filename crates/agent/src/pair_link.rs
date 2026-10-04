//! Helpers shared by discovery and pairing: the host certificate fingerprint and which
//! network interfaces are worth announcing on.

use std::io::BufReader;
use std::net::Ipv4Addr;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Lower-case hex SHA-256 of the first certificate in `cert_pem` (DER bytes).
pub fn cert_fingerprint(cert_pem: &str) -> Result<String> {
    let mut reader = BufReader::new(cert_pem.as_bytes());
    let der = rustls_pemfile::certs(&mut reader)
        .next()
        .context("no certificate in PEM")?
        .context("invalid certificate PEM")?;
    let digest = Sha256::digest(der.as_ref());
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether an interface address is worth putting in the link: IPv4, not loopback,
/// not link-local, and not a container/virtual bridge (the tablet cannot reach those).
pub fn is_usable_interface(name: &str, ip: Ipv4Addr) -> bool {
    if ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() {
        return false;
    }
    const VIRTUAL_PREFIXES: [&str; 6] = ["docker", "br-", "veth", "virbr", "lxc", "podman"];
    !VIRTUAL_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Names of interfaces that have NO address worth announcing (loopback, Docker/veth
/// bridges, link-local only). mDNS announcements exclude these so other devices are
/// never handed an address they cannot reach (e.g. `172.18.0.1` on a Docker bridge).
/// An interface that has at least one usable address is kept.
pub fn unreachable_interface_names(ifaces: &[(String, Ipv4Addr)]) -> Vec<String> {
    let mut usable: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (name, ip) in ifaces {
        if is_usable_interface(name, *ip) {
            usable.insert(name.as_str());
        }
    }
    let mut out: Vec<String> = Vec::new();
    for (name, _) in ifaces {
        if !usable.contains(name.as_str()) && !out.contains(name) {
            out.push(name.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_sha256_of_the_der() {
        let cert = rcgen::generate_simple_self_signed(vec!["nexus".to_string()]).unwrap();
        let pem = cert.cert.pem();
        let expected: String = Sha256::digest(cert.cert.der().as_ref())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(cert_fingerprint(&pem).unwrap(), expected);
        assert_eq!(expected.len(), 64);
    }

    #[test]
    fn fingerprint_rejects_non_certificates() {
        assert!(cert_fingerprint("not a certificate").is_err());
    }

    #[test]
    fn interface_filter_drops_unreachable_addresses() {
        let lan = Ipv4Addr::new(192, 168, 1, 5);
        assert!(is_usable_interface("wlp3s0", lan));
        assert!(is_usable_interface(
            "enp0s25",
            Ipv4Addr::new(10, 20, 245, 1)
        ));
        assert!(!is_usable_interface("lo", Ipv4Addr::LOCALHOST));
        assert!(!is_usable_interface(
            "wlp3s0",
            Ipv4Addr::new(169, 254, 3, 4)
        ));
        assert!(!is_usable_interface(
            "docker0",
            Ipv4Addr::new(172, 17, 0, 1)
        ));
        assert!(!is_usable_interface(
            "br-1a2b3c",
            Ipv4Addr::new(172, 18, 0, 1)
        ));
        assert!(!is_usable_interface("veth12", lan));
    }

    #[test]
    fn unreachable_interfaces_are_listed_once_and_usable_ones_kept() {
        let ifaces = vec![
            ("lo".to_string(), Ipv4Addr::LOCALHOST),
            ("wlp2s0".to_string(), Ipv4Addr::new(172, 16, 48, 177)),
            ("br-a523ef164fb7".to_string(), Ipv4Addr::new(172, 18, 0, 1)),
            ("docker0".to_string(), Ipv4Addr::new(172, 17, 0, 1)),
            ("docker0".to_string(), Ipv4Addr::new(172, 17, 0, 2)),
            ("eth0".to_string(), Ipv4Addr::new(169, 254, 1, 1)),
            // One interface with a bad AND a good address stays announced.
            ("enp0s25".to_string(), Ipv4Addr::new(169, 254, 9, 9)),
            ("enp0s25".to_string(), Ipv4Addr::new(10, 0, 0, 5)),
        ];
        assert_eq!(
            unreachable_interface_names(&ifaces),
            vec!["lo", "br-a523ef164fb7", "docker0", "eth0"]
        );
    }
}
