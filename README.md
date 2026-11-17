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

The time each cached file was last requested is saved in
`state_dir/access.json`, so it survives restarts.

#### What happens to packages a machine stops using

pacoflexo doesn't know what is installed on your machines; it only sees
requests. Packages leave the cache in two ways:

- **Superseded versions** go as soon as they fall outside `keep_versions`,
  whether or not some machine still has them installed.
- **Packages nobody asks for any more** go after `unaccessed_days` without a
  request. If you uninstall something everywhere, nothing requests it again
  and it ages out. A package that was dropped from the repositories is kept in
  its last version until it, too, goes unrequested for that long, so machines
  that still have it can reinstall it in the meantime.

A machine that stays offline for longer than `unaccessed_days` simply
downloads again whatever was dropped the next time it upgrades. Set
`unaccessed_days = 0` to turn age-based removal off and rely on version
retention alone.

### Seeding from an existing cache

Most hosts that run pacoflexo already have a pacman cache full of packages.
`[seed]` lets pacoflexo use those files instead of downloading them again:

```toml
[seed]
dirs = ["/var/cache/pacman/pkg"]
scan_on_start = true
```

On a cache miss, pacoflexo looks for the exact file name in each seed
directory before it goes upstream. It ignores `*.part` files, temp names and
0-byte files. A candidate is first copied into pacoflexo's own cache under a
temp name, using a reflink where the filesystem supports one (btrfs, XFS),
else a hardlink, else a plain copy. pacoflexo then verifies **the copy**
exactly as it would verify a download: size and sha256 against the sync db,
and the signature from `%PGPSIG%`, the seed directory's `.sig` or the mirror.
Only then is the copy renamed into place. A seed file that fails is skipped
and the package is fetched upstream as usual. With `scan_on_start`, a
background task imports every verified current package from the seed
directories once at startup, without delaying serving. `/status` reports
`seed_hits` and `seed_rejects`.

Seed directories are strictly **read-only** to pacoflexo. It never writes,
renames or deletes anything in them, not even a file that fails verification,
because they belong to someone else (usually pacman). That keeps the two caches
independent: pacman can download, clean or partially write its cache at any
time without affecting pacoflexo. The packaged systemd unit also marks
`/var/cache/pacman/pkg` read-only, so the kernel enforces this.

#### Filesystems and disk space

How cheap seeding is depends on the filesystem under `cache_dir` and the seed
directory:

| Filesystem | Import method | Extra disk space |
|---|---|---|
| btrfs, or XFS with `reflink=1` (the mkfs default) | Reflink: the copy shares data blocks with the original | None until one of the two files changes |
| ext4, or different filesystems for `cache_dir` and seed | Plain copy | One extra copy of every seeded package |

A hardlink is tried between the two. In practice it only succeeds when
pacoflexo runs as the owner of the seed files outside the packaged unit: with
the default `fs.protected_hardlinks=1`, the `pacoflexo` user may not link
root-owned pacman files, and the unit's read-only mount of
`/var/cache/pacman/pkg` is a separate mount, which hardlinks can't cross. The
verification is the same in every case, because the imported file is what
gets checked.

On ext4, `scan_on_start = true` copies roughly all current packages from the
seed directory at startup, which doubles their disk use. If space is tight:

1. keep `scan_on_start = false`, so only packages a client actually requests
   are copied, on first request;
2. point the host's own pacman at pacoflexo (see below) and keep pacman's own
   cache small, e.g. with `paccache -rk1`. pacoflexo then holds the one large
   cache, and pacman's is just a staging area.

Tip: point the host's own pacman at pacoflexo as well
(`Server = http://127.0.0.1:9129/repo/archlinux/$repo/os/$arch` at the top of
`/etc/pacman.d/mirrorlist`). The host then benefits from the shared,
verified cache, and new packages reach the LAN cache the first time the host
upgrades.

## URL compatibility

| Style | Client `Server =` line |
|---|---|
| pacoloco | `http://cache.example.lan:9129/repo/archlinux/$repo/os/$arch` |
| flexo | `http://cache.example.lan:7878/$repo/os/$arch` |

Both styles work on every listen address. `pacoloco_prefixes` sets which
`<name>` values are accepted in `/repo/<name>/…` (default `archlinux`). Any
repository name is proxied, but only `repos` get background db refreshes.

`GET /status` returns JSON with cache size, hits, misses, db fetches, seed
hits and rejects, verification failures, upstream errors, in-flight downloads,
index sizes and the current mirror ranking. It reports aggregate numbers only:
no package names and nothing about individual clients.

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
5. pacoflexo starts with an empty cache in its own directory. To reuse
   packages that are already on the host, add the old cache directories to
   `[seed] dirs`. They are only ever read, and every file is verified before
   it is used. Don't point `cache_dir` at an existing cache or at
   `/var/cache/pacman/pkg`.
6. If you used pacoloco's prefetch, set `[prefetch] enabled = true`. Prefetch
   then follows what your clients request through pacoflexo from that point on.

To check the switch, from any client:

```sh
sudo pacman -Syy                                    # databases through the proxy
d=$(mktemp -d)
sudo pacman -Sw --noconfirm --cachedir "$d" <some package>   # download through the proxy
for f in "$d"/*.pkg.tar.zst; do pacman-key --verify "$f.sig" "$f"; done
curl -s http://cache.example.lan:9129/status        # hits, misses, seed_hits
```

and check that `curl -sI http://cache.example.lan:7878/core/os/x86_64/core.db`
answers `200` for flexo-style clients. With `scan_on_start`, the journal
(`journalctl -u pacoflexo`) shows each seeded file and how it was imported
(`Reflink`, `Hardlink` or `Copy`).

To roll back: `systemctl disable --now pacoflexo && systemctl enable --now
flexo pacoloco`. pacoflexo never touched their caches.

## Roadmap

Planned for 0.1.2. Every feature below can be switched on or off on its own
in `/etc/pacoflexo/config.toml`. With all of them off, pacoflexo behaves
exactly like 0.1.1, and host tracking stays off until you enable it.

- **Cache statistics:** bytes served from cache versus fetched upstream, hit
  ratio, per-repository cache size, and an optional Prometheus `/metrics`
  endpoint.
- **Knowing which machine asked:** clients can be identified by a name in
  their mirror URL (`Server = http://cache.example.lan:9129/h/<name>/repo/archlinux/$repo/os/$arch`)
  or by IP address. From the downloads alone, pacoflexo works out which
  version of each package a machine most likely has installed, so retention
  can keep the versions that recently seen machines are still running, and
  forget machines that haven't been seen for a while.
- **Full upgrades, defined by you:** a database fetch and the downloads that
  follow it form a session. Whether a session was a full upgrade
  (`pacman -Syu`) is decided by rules in the config, for example:

  ```toml
  [[hosts.full_upgrade]]
  name = "many unrelated packages"
  min_unrelated = 10      # clusters with nothing in common

  [[hosts.full_upgrade]]
  name = "kernel plus a large batch"
  any_of = ["linux", "linux-lts", "linux-zen", "linux-hardened"]
  min_packages = 20
  ```

  A session is a full upgrade if any rule matches; every condition you set
  inside a rule must hold. Which packages count as related (shared pkgbase,
  group, dependency or provides) is configurable too, and a dry-run command
  explains which rule matched and why before you let any of it affect
  retention.
- **An optional client hook:** a small pacman hook that reports a machine's
  installed packages after each transaction, for exact retention instead of
  inference.

Privacy is a hard requirement for all of this: **which packages are on which
machine is never exposed over the network.** `/status` and `/metrics` will
only ever show aggregate counts, with no package names, no package labels, and
machine names hidden unless you opt in. Per-machine details are available
only to root on the cache host, through a local Unix socket and the
`pacoflexo` command. State files are readable only by the service, and
package names reach the logs only at debug level.

Any shared cache answers faster for files it already has, so in principle a
client could time requests to guess what is cached. For people who use
pacoflexo purely as an offline cache, an opt-in `uniform_speed` mode will
serve every response at the same configured rate. It is off by default.

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
