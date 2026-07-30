# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.1.1] - 2026-04-10

### Fixed

- Cargo.toml formatting
- Set `O_NONBLOCK` on seqpacket listen socket

### Added

- Export `UnixSeqpacketListenerStream`

## [1.1.0] - 2026-04-09

### Added

- Support for systemd-managed Unix `SOCK_SEQPACKET` sockets

## [1.0.1] - 2026-02-10

### Fixed

- Make unix socket non-blocking
- Release procedure

### Changed

- Add badges to README

## [1.0.0] - 2026-01-12

### Added

- Initial public release of `ecdysis`
- GitHub CI pipeline to build and test
- Graceful restart / socket inheritance (tableflip-inspired)

### Fixed

- cargo-sort lint

### Changed

- Style: cargo fmt, clippy lints

[Unreleased]: https://github.com/cloudflare/ecdysis/compare/v1.1.1...HEAD
[1.1.1]: https://github.com/cloudflare/ecdysis/compare/v1.1.0...v1.1.1
[1.1.0]: https://github.com/cloudflare/ecdysis/compare/v1.0.1...v1.1.0
[1.0.1]: https://github.com/cloudflare/ecdysis/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/cloudflare/ecdysis/releases/tag/v1.0.0
