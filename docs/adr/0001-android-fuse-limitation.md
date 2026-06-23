# ADR 0001: Android cannot be a FUSE client

## Status
Accepted

## Context
The original "full device virtualization" concept assumed any device
could mount any other device's filesystem symmetrically. Android
breaks this assumption.

Android did historically use FUSE internally (e.g. `sdcardfs`), but
there is no public API allowing a third-party, non-root app to
register a FUSE filesystem that *other* apps can browse as a real
mount point. Scoped storage (Android 10+) further restricts what an
app can read even within its own process — arbitrary path access
outside the app's sandbox requires the Storage Access Framework (SAF)
and explicit user consent per directory tree, not a blanket grant.

## Decision
Nexus treats device roles asymmetrically:

- **Host role** (serving files out): every platform supports this,
  including Android — read its own accessible storage (via SAF where
  required) and serve it over `FileService` (gRPC). The `nexus-agent`
  crate implements this for all targets.
- **Client/mount role** (mounting someone else's files in via FUSE):
  only Linux, macOS (via macFUSE), and — eventually — Windows (via
  WinFsp) support this. The `nexus-fs` crate is gated to
  `cfg(any(target_os = "linux", target_os = "macos"))` and will not
  compile for Android targets.

On Android, "accessing a remote device's files" is implemented as a
plain in-app file browser UI that calls the same `FileService` gRPC
client used internally by `nexus-fs` — just rendered as a list view
instead of mounted as a kernel-level filesystem. This is a strictly
simpler problem (no inode table, no kernel callback bridging) and
should be a separate crate (`nexus-android-browser` or similar,
not yet created) rather than bolted onto `nexus-fs`.

## Consequences
- Milestone 1 (phone → Dell) is the *correct first milestone* precisely
  because it's the direction that has a real FUSE mount on the
  receiving end. Dell → phone, if built, will look and feel different
  (an app screen, not a mount point) and should not be scoped as "the
  same feature on the other platform."
- Any future teammate proposing "just mount the Dell's files on the
  phone too" should be pointed at this ADR before re-litigating it.
