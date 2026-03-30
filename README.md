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
| `--workers` | CPU count | Number of parallel builds |
| `--dockerfile` | `Dockerfile` | Dockerfile path relative to repo root |
| `--push` | `false` | Push images to the registry after building |

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

1. `git ls-remote --heads` lists all remote branches with no local clone.
2. Branches are filtered by name: `main`, `master`, or containing a Jira ticket ID.
3. A worker pool (default: one worker per CPU) pulls from a job queue.
4. Each worker does a `--depth=1` single-branch clone into a temp directory, runs `docker build`, and optionally `docker push`.
5. Temp directories are removed automatically after each build.
