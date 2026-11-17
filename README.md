# pacoflexo

A small, verifying caching proxy for Arch Linux package repositories, written
in Rust.

Point the machines on your network at pacoflexo instead of a public mirror. The
first machine to ask for a package downloads it through the proxy; every
machine after that gets it from the local cache at LAN speed. Before a package
enters the cache, pacoflexo checks it against the repository's sync database
and the Arch signing keys, so the cache only ever holds files that pacman
would accept.

## Standing on the shoulders of flexo and pacoloco

pacoflexo exists because of two excellent projects, and its name says so:

- **[flexo](https://github.com/nroi/flexo)** picks the fastest mirror for you.
  It measures mirror latency, keeps a ranked list, and moves on to the next
  mirror when one misbehaves. You set it up once and stop thinking about
  mirrorlists.
- **[pacoloco](https://github.com/anatol/pacoloco)** is a simple, dependable
  pacman cache. It is easy to set up for several repositories, and it can
  prefetch: it keeps the packages your machines actually use up to date
  before anyone asks for them.

Many people run them together, with pacoloco in front of flexo, and they work
well that way. If flexo and pacoloco are the chocolate and the peanut butter,
pacoflexo tries to be the peanut butter cup: one service that combines flexo's
mirror selection with pacoloco's caching and prefetch, so there is one process,
one cache and one config file to look after.

Bringing them together in one process also lets pacoflexo do a few things that
need one service to own the whole download path:

| | pacoflexo |
|---|---|
| **Verification before caching** | Every package is checked against the sync database (`%CSIZE%` and `%SHA256SUM%`) and its detached OpenPGP signature is checked against the Arch keyring, before it is written to the cache. A file that fails is deleted and fetched again from the next mirror. Empty or short files are never cached. |
| **Atomic writes** | Downloads go to a temp file in the cache directory and are `rename()`d into place only after they pass verification. A reader never sees a half-written file. |
| **Single-flight downloads** | However many clients ask for the same file at once, there is only one upstream download. Its bytes are streamed to every waiting client as they arrive. |
| **Its own cache directory** | `/var/cache/pacoflexo` belongs to pacoflexo alone and is never shared with the host's `/var/cache/pacman/pkg`. The config refuses that path. |
| **Startup scrub** | On start, every cached file is checked again against the current sync databases (size + sha256) or its signature. Anything that doesn't pass is dropped. |
| **Both URL styles, any number of ports** | pacoloco-style and flexo-style URLs are served side by side, so existing clients of either keep working unchanged. |

## How it works

### Packages

1. A request for `foo-1.0-1-x86_64.pkg.tar.zst` that isn't cached starts (or
   joins) a single download.
2. The first mirror's `Content-Length` must match `%CSIZE%` from the sync db.
   If it doesn't, pacoflexo moves to the next mirror before streaming
   anything.
3. Bytes are written to `cache_dir/.tmp/…` and streamed to every waiting
   client. The **last byte is held back** until verification is finished, so
   a client never receives a complete-looking file that failed.
4. When the download is complete, pacoflexo checks the sha256 against
   `%SHA256SUM%` and the signature (taken from `%PGPSIG%` in the db, or from the
   `.sig` file on the mirror) against the keyring. Then it fsyncs the file and
   `rename()`s it into place, with the `.sig` next to it.
5. If the connection drops part-way through, the download continues from the
   next mirror and waiting clients carry on from the same offset (the file
   name pins the content). If the bytes turn out to be wrong (sha256 or
   signature mismatch), those clients' connections are aborted, so pacman
   reports a failed download rather than a corrupt package. The download is
   then retried from the next mirror for the cache.

Packages that are in no current sync db, such as older versions, are cached
after a `Content-Length` check and signature verification. Cached files are
served with full HTTP `Range` support.

### Databases

`*.db`, `*.files` and their `.sig` files are passed through. pacoflexo stores
them atomically and serves them for `db_ttl_secs` (default 60 s) before it
revalidates upstream with `If-Modified-Since`. If a mirror returns a database
older than the cached copy, pacoflexo skips that mirror. An upstream 404 is
passed through as a 404 and nothing is cached; this matters for `.db.sig`,
which Arch does not publish. Each downloaded `<repo>.db` is parsed (zstd, gzip
or plain tar) and becomes the index that packages are verified against.

### Signature verification

Signatures are checked in-process with [rPGP](https://github.com/rpgp/rpgp)
(the `pgp` crate), which is pure Rust: no gpg, no agent, and no keyring
database to keep in sync. pacoflexo loads `archlinux.gpg` from the
`archlinux-keyring` package and leaves out every key listed in
`archlinux-revoked`. It accepts a signature from any remaining key. This is the
same trust model as `sqv --keyring archlinux.gpg`. Because the keyring comes
from a signed package, it contains only Arch master and packager keys. When
`archlinux-keyring` is upgraded on the host, pacoflexo notices that the files
changed and reloads them before its next verification, without a restart.

### Mirrors

Candidates come from `mirrors.urls` and, if enabled, the
[Arch mirror status JSON](https://archlinux.org/mirrors/status/json/). From the
JSON, pacoflexo keeps only active, fully synced https mirrors, optionally
filtered by country, and takes the best `max_candidates` by score. Every
candidate is then timed by fetching `core.db`, and the list is ordered by that
latency. Requests try mirrors in that order and fall back to the next one on
any error, 404 or failed verification. The ranking is saved in
`state_dir/mirrors.json` and refreshed every `rank_interval_secs`.

### Retention and prefetch

On every `refresh_interval_secs`, pacoflexo:

1. refreshes the dbs of the configured `repos`;
2. applies retention: for each package it keeps the version in the sync db,
   anything newer, and `keep_versions` older versions, ordered by `vercmp`.
   It also deletes packages nobody has requested for `unaccessed_days`;
3. optionally prefetches: for every package a client requested in the last
   `max_age_days`, it downloads the current version if that isn't cached yet.

## URL compatibility

| Style | Client `Server =` line |
|---|---|
| pacoloco | `http://cache.example.lan:9129/repo/archlinux/$repo/os/$arch` |
| flexo | `http://cache.example.lan:7878/$repo/os/$arch` |

Both styles work on every listen address. `pacoloco_prefixes` sets which
`<name>` values are accepted in `/repo/<name>/…` (default `archlinux`). Any
repository name is proxied, but only `repos` get background db refreshes.

`GET /status` returns JSON with cache size, hits, misses, db fetches,
verification failures, upstream errors, in-flight downloads, index sizes and
the current mirror ranking.

## Configuration

`/etc/pacoflexo/config.toml` (override with `-c PATH`; `--check` validates it
and exits). Every key is optional. See
[`config.example.toml`](config.example.toml) for all keys and defaults:

```toml
listen = ["0.0.0.0:9129", "0.0.0.0:7878"]
cache_dir = "/var/cache/pacoflexo"
state_dir = "/var/lib/pacoflexo"
repos = ["core", "extra", "multilib"]
db_ttl_secs = 60

[mirrors]
urls = ["https://geo.mirror.pkgbuild.com/"]
use_status_json = true
countries = ["DE"]

[retention]
keep_versions = 1
unaccessed_days = 30

[prefetch]
enabled = true
```

Logging uses `tracing`; set `RUST_LOG=debug` for more detail.

## Installing

On Arch, build the package from [`packaging/arch/`](packaging/arch/) with
`makepkg -si`. That PKGBUILD builds the tagged GitHub release and is suitable
for the AUR. It installs a hardened systemd unit, a dedicated `pacoflexo`
user, `/var/cache/pacoflexo`, `/var/lib/pacoflexo`, and a default
`/etc/pacoflexo/config.toml` (kept across upgrades).

```sh
sudo systemctl enable --now pacoflexo
```

From source: `cargo build --release --locked`, then run
`target/release/pacoflexo -c config.toml`.

## Moving from flexo + pacoloco

1. Install pacoflexo. Set `listen` to the ports your clients already use
   (pacoloco's default is 9129, flexo's is 7878), and copy your mirror
   preferences into `[mirrors]`.
2. Stop and disable both services, e.g.
   `systemctl disable --now pacoloco flexo`.
3. `systemctl enable --now pacoflexo`, then check `curl -s localhost:9129/status`.
4. Clients need no changes: both URL styles keep working.
5. pacoflexo starts with an empty cache in its own directory. You can remove
   the old caches whenever you like. Don't point `cache_dir` at an existing
   cache or at `/var/cache/pacman/pkg`.
6. If you used pacoloco's prefetch, set `[prefetch] enabled = true`. Prefetch
   then follows what your clients request through pacoflexo from that point on.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The integration tests (`tests/fake_mirror.rs`) start two local fake mirrors. One
serves truncated, corrupted, connection-dropping and 0-byte files, and the other
serves good ones. The tests check that nothing bad is cached, that the fallback
mirror is used, and that concurrent clients share one download. The
signature fixtures in `tests/fixtures/` were made with throwaway keys.

## Contributing

Issues and pull requests are welcome. Please keep changes small and readable:
pacoflexo aims to stay a thin proxy. Before you open a PR, run `cargo fmt`,
`cargo clippy --all-targets -- -D warnings` and `cargo test`, and add a test
for any behavior you change. Add user-visible changes to `CHANGELOG.md`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in this work, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.
