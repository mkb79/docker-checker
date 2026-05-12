# docker-checker

Checks running Docker containers (and optionally stopped containers and locally stored images)
against their registries for available image updates. Read-only — no pulls, no restarts.

## Install

Download the latest binary for your platform from the
[Releases](https://github.com/mkb79/docker-checker/releases) page.

```sh
# Linux x86_64 (static musl — works on any distro)
curl -L https://github.com/mkb79/docker-checker/releases/latest/download/docker-checker-x86_64-linux.tar.gz \
  | tar xz
chmod +x docker-checker
```

Or build from source — see [docs/DOCUMENTATION.md](docs/DOCUMENTATION.md#building-from-source).

## Usage

```sh
# Check all running containers
docker-checker

# Also show up-to-date results
docker-checker -v

# Include stopped containers and standalone images
docker-checker --all

# Combine flags
docker-checker --all -v
```

## Flags

| Flag | Description |
|---|---|
| `-v` / `-vv` / `-vvv` | Verbosity: `-v` shows up-to-date items, `-vv` adds operational info, `-vvv` adds full HTTP/auth detail |
| `-a`, `--all` | Also check stopped containers and locally stored images without a container |
| `-c`, `--concurrent <N>` | Max parallel registry checks (default: 10) |
| `--no-color` | Disable colored output |

## Output

```
Checking 4 running containers...

[↻] redis (redis:latest) — UPDATE ALREADY PULLED — restart required
      local:  sha256:25dbb04...
      remote: sha256:0c3414...
      info:   https://hub.docker.com/_/redis/tags
[!] myapp (myapp:stable) — UPDATE AVAILABLE
      local:  sha256:abc123...
      remote: sha256:def456...
      info:   https://hub.docker.com/r/myuser/myapp/tags

Summary: 4 checked | 2 updates available (1 restart-only) | 0 errors | 0 local-only

  1 dangling image (140.0 MB virtual)
```

## Exit Codes

| Code | Meaning |
|---|---|
| 0 | All images up to date |
| 1 | Fatal error (Docker socket, permissions) |
| 2 | One or more updates available |

## Docker socket permissions

To run without `sudo`, add your user to the `docker` group:

```sh
sudo usermod -aG docker $USER
# Then log out and back in
```

See [docs/DOCUMENTATION.md](docs/DOCUMENTATION.md) for full documentation.

## License

MIT — see [LICENSE](LICENSE).
