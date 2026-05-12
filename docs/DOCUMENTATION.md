# docker-checker — Documentation

## Table of Contents

1. [How It Works](#how-it-works)
2. [CLI Reference](#cli-reference)
3. [Output Format](#output-format)
4. [Exit Codes](#exit-codes)
5. [Building from Source](#building-from-source)
6. [Docker Socket Permissions](#docker-socket-permissions)
7. [Known Limitations](#known-limitations)

---

## How It Works

docker-checker uses the Docker Registry HTTP API v2 to compare the local image digest of each
running container against the remote manifest digest — without pulling any data.

**Digest comparison flow:**

1. Connect to the local Docker daemon via Unix socket (`/var/run/docker.sock` or `$DOCKER_HOST`).
2. List containers and resolve each image reference to a canonical tag.
3. Look up the local manifest digest from `docker inspect` → `RepoDigests`.
4. Fetch the remote manifest digest with a `HEAD` request to the registry:
   - Sends `Accept` headers for both multi-arch manifest lists and single-arch manifests,
     ensuring the returned digest matches what `docker pull` would record.
5. Compare local vs. remote digest — if they differ, an update is available.
6. If the remote digest is already present in the local image store under a different tag
   (e.g., `redis:8` was pulled while `redis:latest` container still runs the old image),
   the status is **UPDATE ALREADY PULLED — restart required**.

**Authentication** follows the standard Bearer token challenge/response flow:
- Sends an unauthenticated probe request to trigger the `WWW-Authenticate` header.
- Fetches a scoped token from the registry's auth endpoint.
- Tokens are cached per registry+image for the duration reported by `expires_in`.

---

## CLI Reference

```
docker-checker [OPTIONS]
```

### Options

| Flag | Short | Default | Description |
|---|---|---|---|
| `--verbose` | `-v` | off | Repeatable. `-v` shows up-to-date and local-only items. `-vv` adds operational info (container count, token TTL). `-vvv` adds full HTTP/auth detail (auth probes, manifest requests, digests). All levels work in release builds. |
| `--all` | `-a` | off | Include stopped containers and locally stored images that have no associated container. |
| `--concurrent` | `-c` | `10` | Maximum number of parallel registry checks. |
| `--no-color` | — | off | Disable ANSI color codes in output. Useful for log files or CI. |
| `--help` | `-h` | — | Show help. |
| `--version` | — | — | Show version. |

### Examples

```sh
# Default: check running containers, show only updates and errors
docker-checker

# Show all results including up-to-date
docker-checker -v

# Check everything including stopped containers and standalone images
docker-checker --all -v

# Limit concurrency for slow networks
docker-checker -c 3

# No colors for log file
docker-checker --no-color >> /var/log/docker-checker.log

# Full debug output (useful for troubleshooting auth issues)
docker-checker -vvv 2>&1 | less
```

---

## Output Format

### Per-item lines

| Symbol | Color | Meaning |
|---|---|---|
| `[✓]` | green | Image is up to date |
| `[!]` | yellow | Update available — pull and restart needed |
| `[↻]` | cyan | Update already pulled — restart only needed |
| `[?]` | dim | Locally built image, no registry digest available |
| `[✗]` | red | Registry check failed |

Container names are printed in **bold**. Stopped containers show `[stopped]`, standalone images show `[image]` in dimmed text.

Update entries include three detail lines:
```
      local:  sha256:<current digest>
      remote: sha256:<new digest>
      info:   <registry URL to view tags>
```

### Prune hints

After the summary, docker-checker reports images that could be freed:

```
  2 dangling images (280.0 MB virtual)
  1 unused image (456.0 MB virtual)
```

- **Dangling images**: untagged (`<none>`) images not used by any container → `docker image prune`
- **Unused images**: tagged images with no associated container, excluding images that are pending
  a container restart (already-pulled updates) → `docker image prune -a`
- Sizes are marked **virtual** because they include shared layers; actual freed space may be less.

### Summary line

```
Summary: 8 checked | 2 updates available (1 restart-only) | 0 errors | 0 local-only
```

---

## Exit Codes

| Code | Meaning |
|---|---|
| `0` | All checked images are up to date |
| `1` | Fatal startup error (cannot connect to Docker, permission denied) |
| `2` | One or more updates are available |

This makes docker-checker suitable for use in scripts and monitoring systems:

```sh
docker-checker || notify "updates available"
```

---

## Building from Source

### Prerequisites

- Rust toolchain: https://rustup.rs
- For static Linux builds: `musl-libc` toolchain

### Linux x86_64 (static musl — default)

The project is pre-configured for static musl builds via `.cargo/config.toml`.

```sh
# Install musl toolchain (Debian/Ubuntu)
sudo apt-get install musl-tools

# Add Rust target
rustup target add x86_64-unknown-linux-musl

# Build (uses config.toml defaults)
cargo build --release

# Verify static linking
ldd target/x86_64-unknown-linux-musl/release/docker-checker
# → "not a dynamic executable"
```

The resulting binary runs on any Linux distribution (Debian, Ubuntu, Alpine, etc.) without
additional dependencies.

### Linux aarch64 (static musl — ARM servers, Raspberry Pi 4/5)

Use [`cross`](https://github.com/cross-rs/cross) for cross-compilation:

```sh
cargo install cross --git https://github.com/cross-rs/cross
cross build --release --target aarch64-unknown-linux-musl
# Binary: target/aarch64-unknown-linux-musl/release/docker-checker
```

### macOS (x86_64 or Apple Silicon)

No additional setup required — builds as a native dynamic binary.

```sh
# Apple Silicon
cargo build --release --target aarch64-apple-darwin

# Intel Mac
cargo build --release --target x86_64-apple-darwin
```

### Windows (x86_64)

Requires the MSVC toolchain (install Visual Studio Build Tools).

```powershell
rustup target add x86_64-pc-windows-msvc
cargo build --release --target x86_64-pc-windows-msvc
# Binary: target\x86_64-pc-windows-msvc\release\docker-checker.exe
```

Note: On Windows, Docker Desktop exposes its daemon via a named pipe. The binary connects
using bollard's `connect_with_local_defaults()` which handles both Unix sockets and named pipes.

---

## Docker Socket Permissions

By default the Docker socket is only accessible to `root` and members of the `docker` group.

**Option 1 — Add user to docker group (recommended for regular use):**
```sh
sudo usermod -aG docker $USER
# Log out and back in for the change to take effect
newgrp docker  # or start a new shell
```

**Option 2 — Run with sudo:**
```sh
sudo docker-checker
```

docker-checker detects permission errors early and exits with a clear message rather than
producing confusing API errors.

---

## Known Limitations

- **Private images** require an authenticated Docker session. If `docker pull` works for an image,
  the anonymous token flow used by docker-checker will not. A future version may support
  credential helpers.

- **Digest-only image references**: if a container was started with a digest
  (`image@sha256:...`) and the image has no tag, docker-checker attempts to resolve the tag
  from `RepoDigests`. If resolution fails, the image is reported as `local-only`.

- **Non-standard registries**: registries that do not implement Bearer token auth or return
  non-standard `WWW-Authenticate` headers may fail. The error message includes the raw reason.

- **Virtual size reporting**: the prune hint sizes sum all layer sizes including shared layers.
  Actual freed space after `docker image prune` may be significantly less for images that share
  a base (e.g., multiple versions of the same image family).
