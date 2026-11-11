# davfast

Fast CLI WebDAV client and **cloud.mail.ru** bridge for Linux (Rust). Built for the main pain of davfs2 and most clients — **thousands of small files**.

```
davfast upload ~/photos http://webdav.example.com/backup/ -c 32
davfast sync ~/data http://127.0.0.1:8080/            # incremental mirror
davfast mailru upload ./folder /backups               # same path as cloud.mail.ru web UI
davfast serve --backend mailru --listen 127.0.0.1:8080  # WebDAV bridge for davfs2
```

## Why it is fast

| Problem of davfs2 / typical clients | davfast |
|---|---|
| Per file: PROPFIND → PUT → PROPFIND (3 round-trips) | One PROPFIND per directory: a tree of thousands of files is compared in a handful of requests |
| Sequential operations | Parallel pipeline `buffer_unordered` (default 16, configure with `-c 32`, `-c 64`...) |
| New TCP connection per request | keep-alive connection pool + TCP_NODELAY + HTTP/2 to the upstream |
| Directories created one by one | Directory tree is created in batches, levels in parallel |
| Re-uploading an unchanged tree | Skip by size (`--skip-existing`) or precisely by ETag/hash (`.davfast-cache.json`) |
| mail.ru cloud via WebDAV is slow | `mailru` mode repeats the web UI flow: PUT to the upload node + `file/add`, hash deduplication |

Local benchmark (in-memory server, no network): 1000 files in 0.06 s (the pipeline does not serialize). On a real network everything is RTT-bound: at 50 ms a single-threaded client does ~20 files/s, davfast with `-c 32` — hundreds of files/s. A repeated `sync` of an unchanged 320-file tree takes 0.013 s.

## One-line install

```bash
curl -fsSL https://raw.githubusercontent.com/teliy/davfast/main/install.sh | bash
```

The installer checks the system for you (network, git, curl, C compiler, Rust, disk space), installs whatever is missing, builds davfast and shows a menu:

1. **Install / update** — build from source, binary into `/usr/local/bin` or `~/.local/bin` (item 2 is the same with manual path selection).
2. **Set up cloud mounting** — asks for the backend (with explanations: `webdav` is a proxy to any WebDAV server, `mailru` talks to the cloud.mail.ru web API directly), the URL, your login and token (the id.mail.ru "app password", hidden input), and the mount point. It then stores the credentials in `~/.config/davfast/davfast.env` (mode 600), creates a systemd service for the bridge with autostart, configures davfs2 (`use_locks 0`, secrets, an fstab line) and tries to mount the cloud right away.
3. **System checks only** — a dependency report, changes nothing.
4. **Uninstall davfast**.

The interface language is picked automatically (English/Russian). Non-interactive mode for CI: `curl -fsSL https://raw.githubusercontent.com/teliy/davfast/main/install.sh | bash -s -- --yes`. Re-running the one-liner updates the repository and rebuilds the binary. To configure mounting without reinstalling: `install.sh --setup-mount`.

## Manual install

Requires Rust 1.75+ (Debian/Ubuntu: `apt install rustc cargo` or [rustup](https://rustup.rs)).

```bash
git clone https://github.com/teliy/davfast.git
cd davfast
cargo build --release
# binary: target/release/davfast
sudo cp target/release/davfast /usr/local/bin/
```

## Commands

| Command | Description |
|---|---|
| `ls <url>` | list a directory |
| `mkdir <url>` | create a directory (with parents) |
| `rm <url> [-r]` | remove a file/directory |
| `upload <local> <url> [-c N] [--skip-existing] [--dry-run]` | upload a file or a tree (recursive, parallel) |
| `download <url> <local> [-c N] [--skip-existing]` | download a file or a tree |
| `sync <local> <url> [-c N] [--delete] [--dry-run]` | incremental mirror local → remote (ETag cache) |
| `serve --backend webdav\|mailru` | local WebDAV bridge/proxy |
| `mailru ls\|upload\|download\|sync ...` | cloud.mail.ru via its web API |

Credentials: `user:pass@host` right in the URL, or the `DAVFAST_USER` / `DAVFAST_PASS` environment variables.

## Quick start

```bash
# upload a directory with 32 parallel streams
davfast upload ~/backup https://webdav.example.com/dav/backup/ -c 32

# see what would change without sending anything
davfast sync ~/backup https://webdav.example.com/dav/backup/ --dry-run

# incremental mirror (+ delete remote files missing locally)
davfast sync ~/backup https://webdav.example.com/dav/backup/ --delete

# download the whole tree
davfast download https://webdav.example.com/dav/backup/ ~/restored -c 32
```

The incremental cache is written to `<local-dir>/.davfast-cache.json` and excluded from transfers. It remembers the ETag (or mail.ru hash) after each upload — the next `sync` sends only really changed files, even when the size matches.

## cloud.mail.ru

The web interface has its own REST API which is noticeably faster than the WebDAV gateway. Full analysis: [docs/analysis-cloud-mail-ru.ru.md](docs/analysis-cloud-mail-ru.ru.md) (Russian). In short: the file goes via PUT to a dedicated upload node (`cld-uploaderXXX`), then is registered with `POST /api/v2/file/add` carrying a hash; the server deduplicates by hash — content the cloud already stores "uploads" instantly, without transferring bytes.

### Directly (fast path, same as the web UI)

```bash
export MAILRU_LOGIN='user@mail.ru'
export MAILRU_PASSWORD='external-app-password'   # id.mail.ru → Security → passwords for external apps

davfast mailru ls /
davfast mailru upload ~/folder /backups -c 32
davfast mailru sync  ~/folder /backups        # incremental, hash-based
davfast mailru download /backups ~/restored -c 32
```

### Bridge for davfs2 and any WebDAV client

`serve --backend mailru` starts a local WebDAV server that translates PROPFIND/GET/PUT/DELETE/MKCOL/MOVE into the cloud's web API:

```bash
davfast serve --backend mailru --listen 127.0.0.1:8080
# in another terminal:
mkdir -p /mnt/cloud
mount -t davfs http://127.0.0.1:8080/ /mnt/cloud
```

In `/etc/davfs2/davfs2.conf` it is recommended to set `use_locks 0` (the same is advised by official Mail.ru help) — the bridge fakes LOCK/UNLOCK, but the cloud does not need locks.

There is also a plain proxy mode for any WebDAV server — connection pool and HTTP/2 to the upstream:

```bash
davfast serve --backend webdav --url https://webdav.example.com/dav/ --listen 127.0.0.1:8081
```

The bridge can be password-protected: `--user me --pass secret` (Basic auth on the local port).

## Environment variables

| Variable | Purpose |
|---|---|
| `DAVFAST_USER`, `DAVFAST_PASS` | WebDAV credentials when they are not in the URL |
| `MAILRU_LOGIN`, `MAILRU_PASSWORD` | cloud.mail.ru account for `mailru` and `serve --backend mailru` |
| `DAVFAST_MAILRU_AUTH_URL` | override `https://auth.mail.ru/cgi-bin/auth` |
| `DAVFAST_MAILRU_REST_BASE` | override `https://cloud.mail.ru` |
| `DAVFAST_MAILRU_DISPATCHER` | dispatcher method path (default `api/v2/dispatcher`) |
| `RUST_LOG=debug` | verbose logging |

## Status and limitations

- **WebDAV client** (`ls/mkdir/rm/upload/download/sync`, `webdav` backend): covered by integration tests (own in-memory DAV server + a separate Python-stdlib test server, 320–1000 files, byte-for-byte comparison). Use it with any WebDAV server.
- **mailru backend** (`mailru ...`, `serve --backend mailru`): implemented from the open API documentation and sources of [kireevmp/mailru-cloud-api](https://github.com/kireevmp/mailru-cloud-api), [mad-gooze/PyMailCloud](https://github.com/mad-gooze/PyMailCloud) and the [rclone mailru](https://rclone.org/mailru/) backend. **Not verified against the live network** (no test account in the sandbox) — first runs with a real account may reveal response-format nuances; endpoints are overridable via environment variables. An "external app password" is required, not the main one.
- File content is held in memory during upload (the target scenario is small files); mail.ru max file size is 2 GB on free plans.
- Symlinks are skipped during tree walks.
- `serve` caps the PUT body at 1 GiB.

## Development

```bash
cargo test        # 13 tests: unit + e2e (400 files: upload/download/sync/delete/cache) + benchmark
cargo run --release -- --help
tests/webdav_test_server.py   # minimal Python DAV server for manual checks
```

## License

GNU GPL v3.0 — see [LICENSE](LICENSE).
