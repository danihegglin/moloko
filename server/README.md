# moloko server

A self-hosted build server in one binary. It runs on a Mac mini or a Linux box and needs no JVM, plugins or external database.

```sh
make server                                # builds the dashboard and embeds it
./server/target/release/moloko serve       # http://127.0.0.1:8080
```

| Flag / env | Default | |
|---|---|---|
| `--listen` / `MOLOKO_LISTEN` | `127.0.0.1:8080` | There is no auth yet, so keep it on localhost or a private network (e.g. Tailscale). |
| `--data` / `MOLOKO_DATA` | `moloko-data` | SQLite DB, git mirrors, caches, logs, workspaces |
| `--executors` / `MOLOKO_EXECUTORS` | CPU count | Max jobs running at once |
| `--webhook-secret` / `MOLOKO_WEBHOOK_SECRET` | unset | Enables `POST /api/webhooks/github` (push events) |

Requires `git`, and `docker` for jobs that set `image:`.

## Pipelines: `.moloko.yml`

```yaml
jobs:
  build:
    image: eclipse-temurin:21        # optional: run steps in Docker; omit to run on the host
    cache:
      - path: .m2/repository         # restored before, saved after a successful job
        key_files: [pom.xml]
    steps:
      - name: compile
        run: mvn -B -Dmaven.repo.local=.m2/repository compile
  test:
    needs: [build]
    shards: 8                        # 8 parallel instances
    steps:
      - run: ./scripts/test-shard.sh "$MOLOKO_SHARD_INDEX" "$MOLOKO_SHARD_TOTAL"
  package:
    needs: [test]                    # waits for all 8 shards
    steps: [docker build -t api:$MOLOKO_SHA .]
```

Check a file locally with `moloko validate [.moloko.yml]`.

Every step gets these environment variables: `CI`, `MOLOKO_RUN_ID`, `MOLOKO_JOB`, `MOLOKO_REF`, `MOLOKO_SHA`, `MOLOKO_SHARD_INDEX` (0-based) and `MOLOKO_SHARD_TOTAL`.

## How it stays fast

- **Git mirror plus worktrees.** Each project keeps one local mirror. A run does an incremental `git fetch`, and each job checks out with `git worktree add`, with no network clone per job.
- **Copy-on-write caches.** Cache entries are content-addressed by the `key_files` contents. They are restored with APFS `clonefile` on macOS and reflinks on btrfs/xfs, so a multi-GB cache restores almost instantly.
- **Parallel job graph.** Jobs start as soon as their `needs` pass, limited by `--executors`. When a dependency fails, the jobs downstream are skipped.
- **Warm containers.** A Docker job starts one container and runs each step with `docker exec`, instead of starting a container per step.
- **Live logs.** Logs stream over server-sent events, and the UI batches lines per animation frame.

## API

| | |
|---|---|
| `GET/POST /api/projects` | list / create `{name, repo_url}` |
| `POST /api/projects/:id/runs` | trigger `{ref}` (default `HEAD`) |
| `GET /api/runs?project_id=` | list runs |
| `GET /api/runs/:id` | run + jobs |
| `POST /api/runs/:id/cancel` | cancel (kills the step's whole process group) |
| `GET /api/jobs/:id/logs` | SSE stream: `line` events, then `end` |
| `POST /api/webhooks/github` | GitHub push webhook (HMAC-verified) |

## Dashboard development

```sh
cargo run -- serve                 # in server/
npm run dev                        # in dashboard/, proxies /api to :8080
```
