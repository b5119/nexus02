# ADR 0015: mDNS Device Discovery

## Status

Accepted

## Context

ADR 0013 introduced numeric-code pairing so devices can exchange identity certificates over the LAN. After pairing, the initiator still needs the host's IP address to connect — `--host 192.168.1.50:50051`. For a system aiming to feel like a mesh, typing IP addresses is a poor experience: they change (DHCP), they are hard to remember, and discovering available devices requires out-of-band knowledge.

mDNS (Multicast DNS, RFC 6762 — the protocol behind Apple Bonjour and Linux Avahi) allows devices on the same LAN to announce named services and discover each other without a central registry. It is zero-configuration, works on all three target platforms (Linux, macOS, Windows), and requires no infrastructure.

This ADR covers adding mDNS service registration to `nexus-agent` and discovery integration into `nexus-mount` and `nexus-viewer`.

### Scope

| In scope | Out of scope |
|---|---|
| mDNS service registration on agent startup | WAN discovery / relay registry |
| `nexus-agent discover` subcommand (scan + print) | DNS-SD browsing via Avahi/Bonjour system daemons |
| `--discover` flag on nexus-mount and nexus-viewer | mDNS on Android (future work) |
| "Paired" filter — only auto-connect to paired devices | Service registration of viewer/mount (host-only) |
| Pure Rust mdns-sd library (no system deps) | |

## Design Decisions

### 1. Service Type

**Decision:** Register `_nexus._tcp.local` as the service type.

The service type `_nexus` is chosen to be short, unambiguous, and unlikely to collide with existing IANA-registered services. The `_tcp` transport is used (even though gRPC runs over TCP) because mDNS requires a transport label and TCP is the correct one for gRPC/HTTP2.

### 2. Instance Name Format

**Decision:** `<display_name> (<device_id_short>)` where `device_id_short` is the first 8 hex characters of the device UUID.

Example: `"Frank's Laptop (a1b2c3d4)"`. This is human-readable and disambiguates when two devices have the same display name.

### 3. TXT Record Schema

| Key | Value | Example |
|---|---|---|
| `device_id` | Full device UUID | `550e8400-e29b-41d4-a716-446655440000` |
| `nexus_version` | Protocol version (currently `1`) | `1` |
| `paired` | Comma-separated list of paired device_ids known to this agent | `550e8400-e29b-41d4-a716-446655440000,e1f2...` |

The `paired` field allows a client to discover which devices already trust this agent. A client can filter discovery results to only those agents that list the client's device_id in their `paired` field, enabling an informed auto-connect decision.

### 4. Library

**Decision:** Use the `mdns-sd` crate (v0.11), a pure-Rust implementation with no system dependencies.

Alternatives considered:
- **Avahi D-Bus**: Requires Avahi daemon, Linux-only.
- **Bonjour (dnssd)**: macOS framework, not cross-platform.
- **Systemd-resolved**: Linux-only, requires systemd.

`mdns-sd` compiles everywhere, is well-maintained (3.7k GitHub stars), and supports both service registration and discovery with a simple async interface.

### 5. Registration Lifecycle

**Decision:** The service registration lives in a `DiscoveryService` struct that starts registration in its constructor and stops it on drop (RAII).

When `nexus-agent serve` starts, it creates a `DiscoveryService` directly in `host::run` (not in a background tokio task). On agent shutdown, the struct is dropped and the mDNS registration is torn down automatically.

The service is **host-only** — viewer and mount do not register themselves on the LAN.

### 6. Discover Subcommand

**Decision:** `nexus-agent discover [--timeout-secs 10]` scans for `_nexus._tcp.local` services for the given duration and prints:

```
device_id                          | display_name         | address         | port  | paired?
550e8400-e29b-41d4-a716-446655440000 | Frank's Laptop      | 192.168.1.50    | 50051 | yes
f1e2d3c4-b5a6-7c8d-9e0f-1a2b3c4d5e6f | Office Desktop      | 192.168.1.100   | 50051 | no
```

The "paired?" column is determined by checking if the discovered device_id exists in this machine's `PeersStore`.

### 7. Client `--discover` Flag

**Decision:** `nexus-mount mount --discover` and `nexus-viewer --discover` replace `--host <URL>` with automatic resolution.

When `--discover` is set:
1. Scan mDNS for `_nexus._tcp.local` services
2. Filter to those whose `device_id` is in `trusted-certs.json` (paired devices)
3. If exactly one paired host found: connect to it
4. If multiple paired hosts found: print the list and ask which one to use (or use `--host-id <device_id>` to disambiguate)
5. If no paired hosts found: print error with instructions to pair first

### 8. Known Limitations

- **LAN only**: mDNS does not route across subnets. WAN discovery requires a future relay/registry.
- **No service registration for clients**: viewer and mount do not advertise themselves.
- **Race condition on startup**: service registration may take ~100ms to propagate; immediate discovery may miss the agent. The `--timeout-secs` parameter mitigates this.
- **Single network interface**: mdns-sd binds to all interfaces by default; no interface filtering is implemented.

## References

- ADR 0013: Device Pairing Protocol
- RFC 6762: Multicast DNS
- mdns-sd crate: https://crates.io/crates/mdns-sd
