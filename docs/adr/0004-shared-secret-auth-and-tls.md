# ADR 0004: Shared-secret auth + self-signed TLS on the data plane

## Status
Accepted

## Context
Through milestone 1 the `FileService` gRPC API had **zero authentication** and
ran in plaintext: anything on the network that could reach the agent's port
could read every file under `--serve-dir`, and traffic was unencrypted. That
was acceptable only as a "prove the data path works" step (see ADR 0001/0002
and the README status). Before adding write support — which would let an
unauthenticated peer *modify or delete* files — the data plane needs at least
a basic auth + encryption layer.

This ADR covers a deliberately minimal step: a shared secret over TLS. It is
**not** the full control-plane pairing design (device identity exchange,
per-device revocation, key rotation) envisioned in the original architecture.

## Decision

### Shared-secret token
- The agent generates a random 128-bit token (rendered as 32 hex chars) on
  first run and persists it in `agent.json` via the existing `AgentConfig`
  mechanism — the same persistence path as `DeviceId` (ADR 0003 covers where
  that lives and the `NEXUS_CONFIG_DIR` override). Configs written before this
  change are migrated: the token is backfilled and saved on next load.
- The client (`nexus-mount`) takes the token via `--token` or the
  `NEXUS_AUTH_TOKEN` env var and sends it as the `x-nexus-token` gRPC metadata
  header on **every** call.
- The host enforces auth in a tonic **interceptor**, which runs before any
  `FileService` method dispatches — so a missing/incorrect token is rejected
  with `Status::unauthenticated` *before* reaching `FileServiceImpl::resolve`
  or touching the filesystem. The comparison is constant-time to avoid leaking
  the token via timing.

### TLS
- The agent generates a **self-signed** certificate (via `rcgen`, `ring`
  backend) on first run, with SANs `localhost` and `127.0.0.1`, and persists
  `cert.pem` / `key.pem` next to `agent.json`. It serves TLS using that
  identity (`tonic` `tls` feature, rustls/ring).
- The client verifies the server by trusting that cert directly (the cert acts
  as its own CA): pass `--ca-cert <cert.pem>` or `NEXUS_CA_CERT`. For the
  loopback test that's the same file on disk; for a real phone→Dell run you
  copy `cert.pem` from the agent to the client once.
- The client connects with `https://` and `domain_name("localhost")` so cert
  verification matches the SAN regardless of which IP was dialed.
- `nexus-mount` does an eager `stat("/")` probe at connect time, so a bad token
  or unreachable/again untrusted host fails immediately at mount with a clear
  error instead of surfacing later as an opaque `EIO` on the first `ls`.

## What this explicitly does NOT cover
- **No pairing UX / device identity exchange.** It's one flat shared secret per
  agent, distributed out-of-band (copy the token + cert). No notion of "this
  specific paired device."
- **No per-device revocation.** Revoking access means rotating the agent's
  token, which invalidates *all* clients at once.
- **No key rotation story.** Token and cert are generated once and reused until
  manually deleted from the config dir.
- **No CA / chain of trust.** Each agent is its own self-signed root; clients
  trust certs individually. This does not scale to many devices and is not a
  PKI.
- **Token is stored in plaintext** in `agent.json` (readable by anyone who can
  read that file / the agent's config dir). Fine for LAN-trust; not a secret
  store.
- **TOCTOU in path resolution** remains (see `resolve()` in `host.rs`) — out of
  scope here.

This is "authenticated, encrypted LAN-trust" — a meaningful step up from
plaintext-and-open, and the prerequisite gate before write support, but not the
full control plane. Do not mistake it for that design.

## Consequences
- The plaintext/open mode is gone: `nexus-mount` now requires `--token` and
  `--ca-cert` (or their env vars) and an `https://` remote. The README
  quickstart is updated accordingly.
- The real phone→Dell flow now needs the token + cert copied to the Dell once
  (the agent logs where they live on startup).
- When the real pairing/control-plane work begins, it supersedes this ADR; the
  shared-secret token can become the bootstrap secret for a proper pairing
  handshake rather than the whole story.
