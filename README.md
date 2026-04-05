# moloko

Build Docker images for every relevant branch in a Git repository — in parallel.

Moloko targets two categories of branches:

- **Default branches** — `main` and `master`
- **Jira ticket branches** — any branch whose name contains a ticket ID like `PROJ-123` or `ABC-4567`

Branches are discovered without cloning the repository. Each matching branch is then shallow-cloned and built concurrently using a worker pool.

## Requirements

- Go 1.21+
- `git` in `$PATH`
- `docker` in `$PATH` (and a running daemon)

## Install

```sh
go install moloko@latest
```

Or build from source:

```sh
git clone <this repo>
cd moloko
go build -o moloko .
```

## Usage

```
moloko --repo <url> [flags]
```

| Flag | Default | Description |
|---|---|---|
| `--repo` | _(required)_ | Git repository URL |
| `--image` | repo name | Base name for built images |
| `--workers` | CPU count | Number of parallel git clones |
| `--build-workers` | `1` | Number of concurrent docker builds |
| `--dockerfile` | `Dockerfile` | Dockerfile path relative to repo root |
| `--push` | `false` | Push images to a registry after building |
| `--key` | | Path to SSH private key for private repositories |
| `--port` | | Address to serve the web UI on, e.g. `:8080` |
| `--state` | `.moloko-state.json` | Path to the build-state file (tracks last-built SHAs) |
| `--watch` | `false` | Poll for branch changes and rebuild when new commits arrive |
| `--interval` | `60s` | Polling interval used with `--watch` |
| `--registry` | | Start a built-in OCI registry on this address, e.g. `:5000` (implies `--push`) |
| `--registry-dir` | `.moloko-registry` | Storage directory for the built-in registry |

## Examples

Build images for all relevant branches, 8 at a time:

```sh
moloko --repo https://github.com/acme/api --workers 8
```

Use a custom image name and push to a registry:

```sh
moloko --repo https://github.com/acme/api \
       --image registry.acme.com/api \
       --push
```

Use a Dockerfile in a subdirectory:

```sh
moloko --repo https://github.com/acme/api \
       --dockerfile docker/Dockerfile.prod
```

Build and store images in the built-in OCI registry:

```sh
moloko --repo https://github.com/acme/api \
       --registry :5000
```

Images are pushed to `localhost:5000/<repo>:<branch-tag>` automatically. The registry implements the OCI Distribution Spec v2 and is compatible with `docker pull`, `docker push`, and standard tooling. Data is stored under `.moloko-registry/` in the current directory.

Enable the web UI and watch for changes in CI:

```sh
moloko --repo https://github.com/acme/api \
       --registry :5000 \
       --port :8080 \
       --watch \
       --interval 30s
```

Use a private repository with an SSH key:

```sh
moloko --repo git@github.com:acme/api.git \
       --key ~/.ssh/id_ed25519
```

## Output

Progress is printed as each build completes:

```
Querying branches for https://github.com/acme/api …
Building 4 branch(es) with 8 parallel worker(s):
  main
  feature/PROJ-42-user-auth
  fix/PROJ-99-crash-on-startup
  feature/ABC-7-dark-mode

[ OK ] main                                               -> api:main (41.2s)
[ OK ] fix/PROJ-99-crash-on-startup                      -> api:fix-proj-99-crash-on-startup (43.8s)
[FAIL] feature/ABC-7-dark-mode                           (12.1s) docker build: exit status 1 ...
[ OK ] feature/PROJ-42-user-auth                         -> api:feature-proj-42-user-auth (55.0s)

Done in 55.1s — 3 succeeded, 1 failed
```

The process exits with status `1` if any build fails.

## Image tagging

Branch names are lowercased and non-alphanumeric characters (`/`, `_`, `#`, spaces) are replaced with `-`. For example:

| Branch | Tag |
|---|---|
| `main` | `main` |
| `feature/PROJ-42-user-auth` | `feature-proj-42-user-auth` |
| `fix/ABC-99_crash` | `fix-abc-99-crash` |

## How it works

1. `git ls-remote --heads` lists all remote branches and their commit SHAs without cloning.
2. Branches are filtered by name: `main`, `master`, or containing a Jira ticket ID.
3. SHAs are compared against the state file (`.moloko-state.json`). Only branches with a new commit are built. Any branch whose image is no longer available is also rebuilt, even if the SHA hasn't changed.
4. `main`/`master` are built first, sequentially, to seed the Docker layer cache.
5. Feature branches run in parallel. Each build receives the primary images as `--cache-from` sources.
6. A semaphore (controlled by `--build-workers`) limits concurrent docker builds to prevent overlay storage exhaustion.
7. After each build, `docker builder prune -f` reclaims dangling cache before the next build slot opens.
8. When `--registry` is set, a built-in OCI-compliant registry starts on the given address. The image base is automatically prefixed with the registry host and `--push` is enabled.

## Built-in registry

The `--registry` flag starts a zero-configuration OCI Distribution Spec v2 registry embedded in the moloko binary — no separate container or daemon required.

- Blobs are stored content-addressably under `<registry-dir>/blobs/sha256/`
- Uploads stream directly to disk with a running SHA-256; nothing is buffered in memory
- Cross-repository blob mounting is supported (avoids re-uploading shared layers)
- Range requests are served via `http.ServeContent` for efficient layer pulls
- Manifests are stored by digest; tags are pointer files that resolve to a digest

```sh
# Build and push to built-in registry, then pull from it
moloko --repo https://github.com/acme/api --registry :5000
docker pull localhost:5000/api:main
```
