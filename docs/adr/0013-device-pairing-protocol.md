# ADR 0013: Device Pairing Protocol

## Status

Accepted

## Context

The current authentication setup (ADR 0004) requires manually generating a shared secret token on the host (`nexus-agent --generate-token`), copying it to the initiator, and installing the host's TLS cert. Every new device requires a manual token copy-paste plus a separate cert-pull step. For a system aiming to feel like a mesh, the initial trust bootstrapping should be a single short interaction — type a 6-digit code, done.

This ADR replaces the manual two-step process with a cryptographic handshake gated by a short-lived, one-time-use numeric code. The data plane (file operations, migration) does not change; only the initial trust establishment is replaced. The existing shared-secret path is retained as a fallback.

### Scope

| In scope | Out of scope |
|---|---|
| Numeric-code pairing over LAN | mDNS/ZeroConf device discovery |
| Per-device identity certificates | QR code pairing (future UI work) |
| Trusted-peers store (peers.json) | WAN relay server |
| CLI subcommands for pairing | Changes to FUSE or migration RPCs |
| Shared-secret fallback retention | Removing the old auth path |

## Design Decisions

### 1. Pairing Listener

**Decision:** The host runs a separate unauthenticated "pairing listener" on port **50052**, distinct from the authenticated data-plane gRPC server on port **50051**.

The data-plane server (50051) requires valid client certificates for every RPC (ADR 0004). An unpaired device has no certificate, so it cannot speak to the data plane at all. The pairing listener reuses the agent's existing long-term `cert.pem` / `key.pem` from the config directory — no ephemeral cert is needed because the pairing listener is just a second gRPC server in the same process with the same TLS identity. The pairing listener provides a single, narrow hole: it accepts exactly one RPC (`RequestPair`), validates the numeric code, and if valid, exchanges identity certificates.

The pairing listener is active only during an explicit `--pair-mode` window (default 60 seconds, configurable via `--pair-timeout-secs`). It is NOT permanently open.

**Why separation matters:**
- The data-plane server never needs to handle unauthenticated requests, so its auth logic stays simple (cert-only). Adding an unauthenticated code-accept path to the data-plane server would introduce a permanent attack surface — every data-plane connection would need to branch on "is this a cert or a code?", and a bug in that branch could bypass cert auth entirely.
- Port 50052 is only open during explicit `--pair-mode` windows, not permanently. This limits the attack surface to a known time window.
- The pairing listener is a separate gRPC service (`PairService`) with no access to filesystem operations, migration state, or any data-plane resources. A compromised pairing listener cannot read or write user data.
- Separation of concerns: if the pairing protocol needs to change (e.g. adding a new handshake step), only the pairing listener is affected, not the data-plane server.

Port assignment:

| Port | Service | Auth required | Active |
|---|---|---|---|
| 50051 | Data plane (filesystem) | mTLS (peer cert) or shared-secret token | Always |
| 50052 | Pairing listener | None (gated by short-lived code) | Only during `--pair-mode` |

### 2. Code Generation and Verification

**Decision:** Host generates a cryptographically random 6-digit numeric code using `OsRng` (from `rand::rngs::OsRng`), displays it on stdout, and accepts it on the pairing port. The code is one-time-use, expires after the pair-mode window, and is compared in constant time.

**Generation:**
- Source of randomness: `rand::rngs::OsRng` (kernel entropy pool, auditable CSPRNG path — NOT `rand::random` which may use a weaker seedable PRNG).
- Format: zero-padded 6-digit string, range `000000`–`999999` (1,000,000 possible values).
- The code is an in-memory secret only: never logged, never written to disk. The host displays it on stdout (the terminal) for the human to read and type on the initiator. No log file, metrics endpoint, or debug output leaks the code.

**Verification (three checks):**
1. **Match:** code equals the generated code (constant-time comparison).
2. **Expiry:** current time is within the pair-mode window (default 60 seconds from code generation).
3. **One-time-use:** code has not been previously accepted (after first valid check, the code is invalidated regardless of expiry).

**Constant-time justification — what attack does it prevent?**

Without constant-time comparison, the `==` operator short-circuits on the first mismatched byte. An attacker who can measure response time (e.g. on the same LAN with microsecond-precision timing) can determine how many prefix bytes they guessed correctly. With 6 digits and a 60-second window, a timing side-channel would reduce the effective search space from 1,000,000 possibilities to ~10 × 6 × 10 = 600 trials on average (each digit position requires ~10 tries, and the timing signal reveals how many digits matched). Constant-time comparison (`subtle::ConstantTimeEq`) ensures every comparison takes the same number of CPU cycles regardless of how many digits match, eliminating this attack.

**Brute-force resistance:** Even without the timing attack, 6 digits gives ~900,000 possible values (after excluding codes like `000000`–`000099` for display reasons; the full 000000–999999 range is used). With a 60-second window and a single-failure rate limit on the pairing listener (max ~3 attempts/second in practice), a brute-force attacker has at most ~180 attempts before the window closes — a 0.02% success rate. This is acceptable for LAN pairing where the attacker must already have network access.

### 3. Trust Establishment

**Decision:** After code verification, both sides exchange self-signed certificates (the same `cert.pem` the agent already generates at startup for data-plane TLS). The host adds the initiator's cert to a trusted-peers store; the initiator adds the host's cert to its trusted-certs store.

**Step-by-step:**

1. Initiator connects to host:50052, sends `PairRequest{code, initiator_device_id, initiator_cert_pem, initiator_display_name}`.
2. Host validates code (constant-time match + not expired + not used), marks code as used.
3. Host responds with `PairResponse{accepted: true, host_cert_pem, host_device_id}` (or `accepted: false` with error message).
4. Host appends initiator's `{device_id, cert_pem, paired_at_timestamp, display_name}` to `peers.json`.
5. Initiator writes host's certificate to `trusted-certs.json` under the host's device_id.
6. Initiator can now connect to port 50051 using its own identity cert for TLS verification. The host's data-plane server checks incoming certs against `peers.json` (or falls back to shared-secret token — see §6).

**Does pairing replace the shared-secret token or coexist with it?**

Coexist. The shared-secret path (ADR 0004) remains as a fallback for manual/scripted setups like CI/CD, existing deployments, and the validated phone test. The data-plane server accepts EITHER a valid peer cert (from a paired device) OR a valid shared-secret token. This is a zero-breakage migration: existing setups continue working without changes, and users who prefer the old flow are not forced to adopt pairing.

The two paths are distinguished at connection time:
- **Cert path:** If the TLS handshake produces a client certificate (mTLS), the host extracts the device_id from the cert, looks it up in `peers.json`, and verifies the cert signature matches the stored PEM.
- **Token path:** If the TLS handshake does NOT produce a client certificate (standard TLS, client does not present a cert), the host checks the `x-nexus-token` metadata header against the stored shared-secret token (existing ADR 0004 logic).

A paired device can choose to connect via cert OR via token. The cert path is preferred because it proves device identity without exposing the shared secret. The token path is retained for:
- Devices that haven't been paired
- CI/scripting environments where token-based auth is more ergonomic
- The existing phone test (validated with ADR 0004)

### 4. Trusted-Peers Store

**Decision:** Two JSON files in `$NEXUS_CONFIG_DIR`:

**`peers.json`** (on the host) — maps `device_id` to peer metadata:

```json
{
  "peers": {
    "06f7c1e2-3b4a-5d6e-8f90-123456789abc": {
      "cert_pem": "-----BEGIN CERTIFICATE-----\nMIIB...\n-----END CERTIFICATE-----",
      "paired_at": 1720041600,
      "display_name": "Frank's Phone"
    },
    "a1b2c3d4-e5f6-7890-abcd-ef1234567890": {
      "cert_pem": "-----BEGIN CERTIFICATE-----\nMIIB...\n-----END CERTIFICATE-----",
      "paired_at": 1720128000,
      "display_name": "XPS 15"
    }
  }
}
```

**`trusted-certs.json`** (on the initiator) — maps `device_id` to host metadata:

```json
{
  "hosts": {
    "b7d8e9f0-1a2b-3c4d-5e6f-789012345678": {
      "cert_pem": "-----BEGIN CERTIFICATE-----\nMIIB...\n-----END CERTIFICATE-----",
      "host_display_name": "My Dell"
    }
  }
}
```

Both files are loaded at agent startup. The agent uses `peers.json` to verify incoming mTLS connections; the client (nexus-mount) uses `trusted-certs.json` to know which hosts it has paired with.

**Justification:**
- JSON is human-readable and trivially editable for debugging (removing a paired device, changing a display name).
- Storing the full PEM means the host can re-verify the certificate on every connection, not just at pairing time.
- The file path follows the existing `NEXUS_CONFIG_DIR` convention (ADR 0003), alongside `agent.json`, `cert.pem`, `key.pem`, etc.
- Two separate files because the host and initiator have different data: the host tracks all paired peers, the initiator tracks all paired hosts.

### 5. Threat Model

**What this protocol PREVENTS:**

| Attack | Mitigation |
|---|---|
| Unauthorized device connecting to the data plane | Host verifies cert against `peers.json` on every mTLS connection; unknown certs are rejected |
| Brute-force guessing of the pairing code | 6-digit code (~900k space), 60s window, single-failure rate limit (~180 attempts max = 0.02% success rate) |
| Replay of a captured `PairRequest` | Code is one-time-use — marked used after first success |
| Timing side-channel on code comparison | Constant-time comparison via `subtle::ConstantTimeEq` |
| Eavesdropper learns long-term keys | Code is not key material; cert exchange happens inside the TLS connection on the pairing port |
| Impersonation after pairing | Host verifies cert fingerprint against stored PEM on every data-plane connection |

**What this protocol does NOT prevent (explicit gaps):**

| Gap | Explanation | Severity |
|---|---|---|
| Attacker with the pairing code within the 60s window | If an attacker is on the same network segment and can observe or intercept the 6-digit code (e.g. shoulder-surfing the host's terminal, screen-sharing, compromised terminal emulator) AND can reach port 50052 within the window, they can pair their own device. | Medium — the attacker needs simultaneous network access AND code visibility. This is a social-engineering / physical-proximity attack, not a protocol flaw. |
| Attacker on the same LAN intercepting the TLS connection to port 50052 | The pairing listener uses the same long-term `cert.pem` the agent serves data-plane TLS with — it is NOT ephemeral. A MITM could intercept the `PairRequest` (learning the code) but cannot reuse it (the legitimate request arrives first) and cannot forge the `PairResponse` without the host's private key. The MITM could drop the legitimate request and pair their own device if they also have the code. To mitigate this, both sides should manually verify the other's `device_id` after pairing (e.g. read the "Paired successfully with <device_id>" line on the initiator and confirm it matches the host's device_id shown in `nexus-agent list-peers`). | Medium — the code itself authenticates the initiator to the host, not vice versa. The initiator trusts the host's identity based on the response being signed by the host's private key. Manual device_id verification catches MITM-forged host identities. |
| Physical access to `NEXUS_CONFIG_DIR` | If an attacker can read/write `peers.json`, they can add their own device or modify existing entries. If they can read `agent.json`, they have the shared-secret token. | High — same as any local config tampering. Mitigated by OS file permissions on the config directory. |
| Offline brute-force after the window closes | After the 60s window, the code is discarded. An attacker who captured the encrypted traffic cannot brute-force the code offline because the code is never used as key material. | None — this attack is structurally impossible. |

### 6. Migration from Shared-Secret

Existing setups continue working unchanged. The transition plan:

1. **Existing shared-secret setups:** `--generate-token`, `--token`, and the token-based flow (ADR 0004) all remain functional. `peers.json` is checked first; if the device is not found there, the shared-secret fallback is used.
2. **Mixed setups:** A host can accept connections from paired devices (cert-based) AND unpaired devices (token-based) simultaneously. The two auth paths are independent.
3. **New setups:** Users who want the simpler flow use `--pair-mode` + `--pair`. The old flow remains documented for CI/scripting.
4. **Future removal:** If pairing adoption reaches >95% of deployments, shared-secret could be deprecated and removed in a future major version. That decision is at least 6 months out.

**How the agent distinguishes the two paths on an incoming connection:**

When a TLS connection arrives on port 50051:
1. If the TLS handshake includes a client certificate (mTLS):
   - Extract `device_id` from the certificate's CN (set to `DeviceId` at cert generation time).
   - Look up the device_id in `peers.json`.
   - If found AND the cert signature matches the stored PEM → **cert-authenticated** (no token needed).
   - If not found or signature mismatch → **reject** the connection (do NOT fall through to token check — a client that presents a cert is asserting cert-based identity; if that identity is unknown, it's not allowed to fall back to token auth on the same connection).
2. If the TLS handshake does NOT include a client certificate:
   - Check the `x-nexus-token` metadata header against the stored shared-secret token (existing ADR 0004 logic).
   - If valid → **token-authenticated**.
   - If invalid → **reject**.

This design ensures that a compromised cert does not give access to the token-based path, and a leaked token does not invalidate cert-based access.

## Protocol Details

### Protobuf service definition

```protobuf
// crates/proto/proto/pair_service.proto
syntax = "proto3";
package nexus.pair.v1;

service PairService {
  rpc RequestPair(PairRequest) returns (PairResponse);
  rpc ListPeers(ListPeersRequest) returns (ListPeersResponse);
}

message PairRequest {
  string code = 1;
  string initiator_device_id = 2;
  string initiator_cert_pem = 3;
  string initiator_display_name = 4;
}

message PairResponse {
  bool accepted = 1;
  string host_cert_pem = 2;
  string host_device_id = 3;
  string error_message = 4;
}

message ListPeersRequest {}

message PeerInfo {
  string device_id = 1;
  string display_name = 2;
  int64 paired_at = 3;
}

message ListPeersResponse {
  repeated PeerInfo peers = 1;
}
```

### Sequence diagram

```
Initiator (nexus-mount pair)                Host (nexus-agent pair-mode)
        |                                         |
        |   (user reads 6-digit code from host     |
        |    terminal, types it on initiator)      |
        |                                         |
        |------- TCP connect :50052 ------------->|
        |------- PairRequest{code, id, cert,      |
        |         display_name} ----------------->|
        |                                         | validate code:
        |                                         |   1. constant-time match
        |                                         |   2. not expired (<60s)
        |                                         |   3. not used before
        |                                         | mark code as used
        |                                         | write initiator to peers.json
        |<------ PairResponse{accepted: true,      |
        |         host_cert, host_id} -------------|
        |                                         |
        |   (initiator writes host to               |
        |    trusted-certs.json)                    |
        |                                         |
        |------- TCP connect :50051 ------------->|
        |------- mTLS handshake ----------------->|
        |       (initiator cert → host verifies    |
        |        against peers.json, no token!)    |
        |------- FileService RPCs --------------->|
```

## CLI Changes

### nexus-agent

Add subcommand pattern (replacing current flat args):

```
nexus-agent serve [--serve-dir DIR] [--port PORT]
                 [--gc-interval-hours HOURS]
                 [--tombstone-ttl-hours HOURS]
                 [--max-store-entries COUNT]
    Run the data-plane server (current behavior, now under "serve")

nexus-agent pair-mode [--timeout-secs 60]
                      [--display-name "My Dell"]
    Start the pairing listener on port 50052, display 6-digit
    code on stdout, wait for a PairRequest, exit after pair
    or timeout.

nexus-agent list-peers
    Print paired devices from peers.json as a table:
    device_id | display_name | paired_at
```

### nexus-mount

```
nexus-mount pair --host <address> --code <6-digit-code>
             [--display-name "My Laptop"]
    Connect to host:50052, send PairRequest, on success write
    to trusted-certs.json, print "Paired successfully with
    <host_device_id>".

nexus-mount mount --remote <address> --mountpoint <dir>
             [--token <token> | --trusted]
    Mount the remote filesystem. EITHER use --token (old path,
    ADR 0004) OR --trusted (use trusted-certs.json for this
    host, no token needed).
```

## Crate-level changes

- A new `PairService` proto definition is added to `crates/proto/proto/` alongside `file_service.proto` and `migrate_service.proto`. Tonic codegen includes it.
- The pairing logic lives in `crates/agent/src/pairing.rs` as a new module. The pairing listener runs as part of the agent process (alongside the data-plane server). It reads `peers.json` from the same `NEXUS_CONFIG_DIR`.
- The auth interceptor in `host.rs` gains mTLS cert verification logic alongside the
  existing token check.
- `load_or_create_tls_identity()` in `config.rs` must embed `DeviceId` into the
  self-signed certificate (as CN or SAN) so the interceptor can extract it from
  incoming mTLS connections to look up the peer in `peers.json`.
- New dependencies: `subtle` (constant-time comparison), `rand` (OsRng for code generation).
### 7. Static Peer CA Trust Store

**Decision:** The peer CA trust store (built from `peers.json` at agent startup) is static for the lifetime of the agent process. Devices paired after the agent starts must wait for an agent restart before their certificate-based authentication works via the `client_ca_root()` path. Token-based auth (ADR 0004) works immediately after pairing without restart.

**Known limitation:** The `ServerTlsConfig::client_ca_root()` API in tonic 0.12.x (and 0.14.x) accepts a static `Certificate` bundle. There is no public API to hot-reload the trust anchor store without rebuilding the TLS acceptor. A future enhancement should implement dynamic reload by wrapping the TLS acceptor with a custom `Connected` implementation that can be updated at runtime.

**Known gap — TLS-layer certificate rejection:** Production currently uses the static CA configuration via `client_ca_root()`. The unwired `ClientCertVerifier` in `custom_tls.rs` always accepts certificates without peer-store access — wiring it would not provide TLS-layer rejection. Peer-aware TLS-layer rejection requires a different verifier that has access to the peer store, to be implemented and wired only after the tonic upgrade exposes the required public `Error` type. Tracked in issue #15 (tonic upgrade).

**Issue:** Support dynamic peer CA store reload without agent restart — tracked in GitHub issue #TBD.

## Consequences

- Pairing replaces the manual two-step setup (token + cert-pull) with a single code-typing interaction for most users.
- The existing shared-secret path remains fully functional — zero breakage for existing deployments and the validated phone test.
- Security is bounded by the 6-digit code's entropy, the 60s window, one-time-use semantics, and constant-time comparison. This is appropriate for LAN pairing where the attacker already needs network access.
- The `peers.json` and `trusted-certs.json` files are new on-disk state that users can inspect, back up, or manually edit.
- Port 50052 is a new attack surface but is time-limited and scoped to a single RPC with no data-plane access.
- The threat model is honest for LAN + human-mediated pairing: we assume the human who types the code is authorized, and the network is not actively hostile (the code provides a basic auth layer against casual attackers).

## References

- ADR 0003: Config directory override (NEXUS_CONFIG_DIR convention)
- ADR 0004: Shared-secret auth and TLS (existing auth path, retained as fallback, constant-time token comparison reference)
- Issue #TBD: Implement device pairing protocol (control plane)
