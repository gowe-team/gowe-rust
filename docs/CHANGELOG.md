# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- GitHub issue templates (feature request and bug report) and pull request template.

## [2.0.0] - 2026-05-01

### Added

- New default v2 encoder/decoder module for scalar/dynamic values.
- v2 tag families including fixint/fixstr/fixarray/fixmap and compact integer width tags.
- Per-message key and string interning, plus same-shape map-array shape definition reuse.

### Changed

- Public `encode` / `decode` now use the v2 wire path by default.
- Crate version bumped to `2.0.0` for the clean-break format revision.

## [0.1.0] - 2026-03-23

Initial public release of the Rust implementation of Recurram.

### Added

- Core wire format implementation with dynamic `Value` model and `encode` / `decode` APIs.
- Schema-aware encoding, batch encoding, and session-based micro-batch support.
- Stateful transport features including base snapshots, state patch encoding, template batch handling, control stream support, and trained dictionary support.
- Comprehensive test coverage for spec vectors, dynamic profile behavior, control streams, bound batch stateful flows, and broader codec/protocol scenarios.
- Project documentation, MIT licensing, CI automation, and automated crates.io publishing on version tags.

### Changed

- Updated the release documentation in `README.md` for automated publishing.
- Clarified the README license notice.
- Tuned protocol performance in the initial release line.
- Renamed the spec traceability document to `docs/SPEC-TEST-TRACEABILITY.md`.

### Fixed

- Add missing crates.io package metadata (`description`, `license`) so `cargo publish` succeeds.

[unreleased]: https://github.com/recurram/recurram-rust/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/recurram/recurram-rust/releases/tag/v0.1.0
