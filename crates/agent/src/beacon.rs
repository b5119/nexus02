//! LAN discovery that works where multicast does not.
//!
//! Campus and enterprise Wi-Fi often drops multicast (so mDNS finds nothing) while
//! still passing broadcast and direct traffic between devices. This module speaks a
//! tiny UDP protocol on [`BEACON_PORT`]:
//!
//! * a phone broadcasts the probe `NEXUS1?`; every host that hears it replies, directly
//!   to the sender, with a **beacon**;
//! * each host also broadcasts its beacon every few seconds, so a phone that only
//!   *listens* still sees it.
//!
//! A beacon is `NEXUS1<TAB>device_id<TAB>name<TAB>data_port<TAB>approve_port`. The phone
//! takes the host's **address from the packet's source**, so the address it shows is one
//! it can actually reach. A beacon proves nothing about identity: pairing still needs the
//! approval dialog and matching code in `approval.rs`.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;

/// UDP port for probes and beacons.
pub const BEACON_PORT: u16 = 50054;
/// How often a host announces itself unprompted.
pub const BEACON_INTERVAL: Duration = Duration::from_secs(3);

const MAGIC: &str = "NEXUS1";
const PROBE: &[u8] = b"NEXUS1?";

/// Builds the beacon text. Control characters (tabs, newlines) in `name` are removed so
/// they cannot break the framing, and the name is capped at 40 characters.
pub fn encode_beacon(device_id: &str, name: &str, data_port: u16, approve_port: u16) -> String {
    let name: String = name.chars().filter(|c| !c.is_control()).take(40).collect();
    format!("{MAGIC}\t{device_id}\t{name}\t{data_port}\t{approve_port}")
}

/// A host's announcement as parsed from a beacon. Only tests parse beacons on this side
/// (the phone has its own parser), so this is test-only: it pins the wire format.
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub struct Beacon {
    pub device_id: String,
    pub name: String,
    pub data_port: u16,
    pub approve_port: u16,
}

/// Parses a beacon; `None` if it is not a well-formed Nexus beacon.
#[cfg(test)]
pub fn parse_beacon(payload: &[u8]) -> Option<Beacon> {
    let text = std::str::from_utf8(payload).ok()?;
    let mut parts = text.split('\t');
    if parts.next()? != MAGIC {
        return None;
    }
    let device_id = parts.next()?.to_string();
    let name = parts.next()?.to_string();
    let data_port = parts.next()?.parse().ok()?;
    let approve_port = parts.next()?.parse().ok()?;
    if parts.next().is_some() || device_id.is_empty() {
        return None;
    }
    Some(Beacon {
        device_id,
        name,
        data_port,
        approve_port,
    })
}

/// True if `payload` is a discovery probe.
pub fn is_probe(payload: &[u8]) -> bool {
    payload == PROBE
}

/// Broadcast destinations to announce on: the limited broadcast plus each real
/// interface's directed broadcast (e.g. `172.16.255.255`), never Docker/virtual ones.
pub fn broadcast_targets(ifaces: &[(String, Ipv4Addr, Option<Ipv4Addr>)]) -> Vec<Ipv4Addr> {
    let mut out = vec![Ipv4Addr::BROADCAST];
    for (name, ip, broadcast) in ifaces {
        if let Some(b) = broadcast {
            if crate::pair_link::is_usable_interface(name, *ip) && !out.contains(b) {
                out.push(*b);
            }
        }
    }
    out
}

fn current_targets() -> Vec<Ipv4Addr> {
    let ifaces: Vec<(String, Ipv4Addr, Option<Ipv4Addr>)> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) => Some((i.name, v4.ip, v4.broadcast)),
            _ => None,
        })
        .collect();
    broadcast_targets(&ifaces)
}

/// Binds the discovery socket (all interfaces, broadcast enabled).
pub async fn bind_beacon_socket(port: u16) -> Result<UdpSocket> {
    let socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], port)))
        .await
        .with_context(|| format!("binding UDP discovery port {port}"))?;
    socket
        .set_broadcast(true)
        .context("enabling UDP broadcast")?;
    Ok(socket)
}

/// Answers probes and announces this host until the task is dropped.
/// `announce_port` is where beacons are broadcast to (normally [`BEACON_PORT`]).
pub async fn run_beacon(
    socket: Arc<UdpSocket>,
    announce_port: u16,
    beacon: String,
    interval: Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    let mut buf = [0u8; 256];
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                for target in current_targets() {
                    let dest = SocketAddrV4::new(target, announce_port);
                    let _ = socket.send_to(beacon.as_bytes(), dest).await;
                }
            }
            received = socket.recv_from(&mut buf) => {
                if let Ok((n, from)) = received {
                    if is_probe(&buf[..n]) {
                        let _ = socket.send_to(beacon.as_bytes(), from).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "9c3e58d6-7272-4a01-b4a9-eb950c44642a";

    #[test]
    fn beacon_roundtrips() {
        let text = encode_beacon(ID, "Frank's Dell", 50051, 50053);
        assert_eq!(
            parse_beacon(text.as_bytes()),
            Some(Beacon {
                device_id: ID.into(),
                name: "Frank's Dell".into(),
                data_port: 50051,
                approve_port: 50053
            })
        );
    }

    #[test]
    fn hostile_names_cannot_break_the_framing() {
        let text = encode_beacon(ID, "evil\tname\nwith\u{7}controls", 50051, 50053);
        let parsed = parse_beacon(text.as_bytes()).expect("still parses");
        assert_eq!(parsed.name, "evilnamewithcontrols");
        assert_eq!(parsed.data_port, 50051);
        let long = encode_beacon(ID, &"x".repeat(100), 1, 2);
        assert_eq!(long.split('\t').nth(2).unwrap().len(), 40);
    }

    #[test]
    fn malformed_beacons_are_rejected() {
        for bad in [
            &b""[..],
            b"NEXUS1?",
            b"HELLO\tid\tname\t1\t2",
            b"NEXUS1\tid\tname\t1",
            b"NEXUS1\tid\tname\tnotaport\t2",
            b"NEXUS1\t\tname\t1\t2",
            b"NEXUS1\tid\tname\t1\t2\textra",
            &[0xff, 0xfe, 0xfd],
        ] {
            assert_eq!(parse_beacon(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn only_the_exact_probe_is_a_probe() {
        assert!(is_probe(b"NEXUS1?"));
        assert!(!is_probe(b"NEXUS1? "));
        assert!(!is_probe(b"nexus1?"));
        assert!(!is_probe(b""));
    }

    #[test]
    fn broadcast_targets_skip_virtual_interfaces_and_dedupe() {
        let ifaces = vec![
            (
                "wlp2s0".to_string(),
                Ipv4Addr::new(172, 16, 48, 177),
                Some(Ipv4Addr::new(172, 16, 255, 255)),
            ),
            (
                "docker0".to_string(),
                Ipv4Addr::new(172, 17, 0, 1),
                Some(Ipv4Addr::new(172, 17, 255, 255)),
            ),
            ("lo".to_string(), Ipv4Addr::LOCALHOST, None),
            (
                "wlan1".to_string(),
                Ipv4Addr::new(172, 16, 9, 9),
                Some(Ipv4Addr::new(172, 16, 255, 255)),
            ),
        ];
        assert_eq!(
            broadcast_targets(&ifaces),
            vec![Ipv4Addr::BROADCAST, Ipv4Addr::new(172, 16, 255, 255)]
        );
    }

    #[tokio::test]
    async fn a_probe_gets_a_direct_beacon_reply() {
        let server = Arc::new(bind_beacon_socket(0).await.unwrap());
        let server_addr = server.local_addr().unwrap();
        let beacon = encode_beacon(ID, "Test Host", 50051, 50053);
        // Long interval: only the probe reply can reach the client in this test window.
        tokio::spawn(run_beacon(
            server.clone(),
            1,
            beacon.clone(),
            Duration::from_secs(3600),
        ));

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = SocketAddr::from(([127, 0, 0, 1], server_addr.port()));
        client.send_to(PROBE, target).await.unwrap();

        let mut buf = [0u8; 256];
        let (n, from) = tokio::time::timeout(Duration::from_secs(3), client.recv_from(&mut buf))
            .await
            .expect("reply within 3 s")
            .unwrap();
        assert_eq!(from.port(), server_addr.port());
        assert_eq!(parse_beacon(&buf[..n]).unwrap().name, "Test Host");

        // Unrelated datagrams get no reply.
        client.send_to(b"garbage", target).await.unwrap();
        let silent =
            tokio::time::timeout(Duration::from_millis(400), client.recv_from(&mut buf)).await;
        assert!(silent.is_err(), "no reply to non-probes");
    }
}
