## Summary

<!-- What does this change, and why? Reference the layer/milestone if relevant. -->

## Platform tested

<!-- Nexus spans OSes with different capabilities (see CONTRIBUTING.md) — say where you actually tested. -->

- [ ] Linux (host `nexus-agent` and/or client `nexus-mount`)
- [ ] macOS (macFUSE)
- [ ] Android host (`cargo-ndk`)
- [ ] Not platform-specific

## Related ADR(s)

<!-- Link any docs/adr/ entry this touches or is constrained by. New design decision? Add an ADR. -->

## Checklist

- [ ] Read the relevant ADR(s) in `docs/adr/` (esp. [ADR 0001](docs/adr/0001-android-fuse-limitation.md) before any "make Android mount things" idea)
- [ ] `cargo fmt --all --check` is clean
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] Updated the README / an ADR if this changes a documented behavior or a named gap
