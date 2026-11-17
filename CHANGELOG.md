# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

Planned for 0.2.0: cache statistics, per-machine retention, configurable
full-upgrade detection, an optional shared cache with the host's pacman and
an optional client hook. See the Roadmap section of the README.

## [0.1.2] - 2026-11-17

### Fixed

- Log output no longer contains ANSI colour escape codes when it does not go
  to a terminal (for example in the systemd journal). `NO_COLOR` is honoured
  as well.

### Documentation

- README: retention of unused packages, filesystem notes for seeding
  (reflinks, hardlinks, copies), migration checks, and a roadmap for 0.2.0,
  including a shared cache mode and why upgrade ordering matters there.

## [0.1.1] - 2026-11-17

### Added

- Read-only seed directories (`[seed] dirs`, e.g. the host's
  `/var/cache/pacman/pkg`): on a cache miss, a file with the exact name is
  copied into the cache (reflink, else hardlink, else copy), verified like a
  download (size, sha256, signature) and committed atomically. Seed
  directories are never written to. Optional `scan_on_start` imports every
  verified current package in the background. `seed_hits` and
  `seed_rejects` in `/status`.
- `packaging/arch/`: AUR-style PKGBUILD (GitHub release tarball), hardened
  systemd unit (seed dir mounted read-only), sysusers/tmpfiles entries and
  default config.

### Fixed

- `unreachable_code` warning with newer Rust toolchains.

## [0.1.0] - 2026-11-17

### Added

- Caching proxy for Arch Linux repositories serving both pacoloco-style
  (`/repo/<name>/$repo/os/$arch/<file>`) and flexo-style
  (`/$repo/os/$arch/<file>`) URLs, on any number of listen addresses.
- Package verification before caching: size and sha256 against the sync
  database, detached OpenPGP signature against the Arch keyring (pure Rust,
  honoring the revoked-keys list, reloaded automatically when the keyring
  files change). Atomic `rename()` into the cache.
- Single-flight downloads with streaming to all waiting clients; the last byte
  is held back until verification passes. Transparent resume from the next
  mirror when a transfer breaks off.
- Mirror selection from a configured list and/or the Arch mirror status JSON,
  ranked by latency, persisted in the state directory, with per-request
  fallback.
- Database pass-through with a configurable freshness TTL and
  `If-Modified-Since` revalidation; upstream 404s (e.g. `.db.sig`) are passed
  through and never cached.
- Startup scrub of the cache, version-aware retention (current + N previous,
  unaccessed-TTL purge) and optional prefetch of recently requested packages.
- `/status` JSON endpoint.
