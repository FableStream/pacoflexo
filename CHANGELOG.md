# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-11-17

### Added

- Caching proxy for Arch Linux repositories serving both pacoloco-style
  (`/repo/<name>/$repo/os/$arch/<file>`) and flexo-style
  (`/$repo/os/$arch/<file>`) URLs, on any number of listen addresses.
- Package verification before caching: size and sha256 against the sync
  database, detached OpenPGP signature against the Arch keyring (pure Rust,
  honoring the revoked-keys list). Atomic `rename()` into the cache.
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
