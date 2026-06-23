# ADR 0003: Config directory must be overridable, not just $HOME-relative

## Status
Accepted (validated by real-world testing)

## Context
`AgentConfig::load_or_create()` (`crates/agent/src/config.rs`) needs
somewhere to persist this device's `DeviceId` across restarts. On a
normal Linux/macOS desktop session, `$HOME/.config/nexus/agent.json`
is the obvious choice.

This assumption does not hold in every environment the agent actually
runs in. When manually testing the cross-compiled agent on a real
Android device via `adb shell` (see ADR 0002's "real phone test"
update), `$HOME` resolves to `/`, which is read-only to the shell
user. The agent failed at startup with `Read-only file system (os
error 30)` before ever binding its port.

## Decision
`config_path()` already checked for a `NEXUS_CONFIG_DIR` environment
variable before falling back to `$HOME`-relative resolution — this was
written speculatively, anticipating that Android (and other
non-standard environments) wouldn't have a meaningful `$HOME`. The
real-world failure confirmed the anticipated problem was real, and the
existing escape hatch fixed it with zero code changes:

```bash
NEXUS_CONFIG_DIR=/data/local/tmp/nexus-cfg ./nexus-agent --serve-dir ...
```

## Consequences
- Any future wrapper (Kotlin foreground service, systemd unit, launchd
  plist, etc.) that invokes `nexus-agent` in an environment where
  `$HOME` is unset, unwritable, or not meaningfully tied to "this
  device's persistent storage" **must** set `NEXUS_CONFIG_DIR`
  explicitly rather than relying on the `$HOME` fallback.
- This is a useful pattern to repeat for any future per-device state
  the agent needs to persist (not just `DeviceId`) — don't assume
  `$HOME` is a safe default anywhere this binary might run.
