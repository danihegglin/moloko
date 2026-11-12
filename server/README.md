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
| `--registry-host` / `MOLOKO_REGISTRY_HOST` | `localhost:<port>` | The address jobs and agents use for the built-in registry, e.g. `moloko.tail1234.ts.net` |

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

Every step gets these environment variables: `CI`, `MOLOKO_RUN_ID`, `MOLOKO_JOB`, `MOLOKO_REF`, `MOLOKO_SHA`, `MOLOKO_SHARD_INDEX` (0-based) and `MOLOKO_SHARD_TOTAL`. It also gets the registry variables described below: `MOLOKO_REGISTRY`, `MOLOKO_REGISTRY_USER`, `MOLOKO_REGISTRY_TOKEN` and `MOLOKO_IMAGE`.

## Built-in registry

moloko is also an OCI/Docker registry. It serves the Distribution v2 API at `/v2/` on the same port as everything else. Images a pipeline builds go straight into it, and deploy agents pull them from there, so there's no GHCR or Docker Hub round trip and no extra service to run.

```yaml
jobs:
  image:
    steps:
      - echo "$MOLOKO_REGISTRY_TOKEN" | docker login "$MOLOKO_REGISTRY" -u "$MOLOKO_REGISTRY_USER" --password-stdin
      - docker build -t "$MOLOKO_IMAGE/api:$MOLOKO_SHA" . && docker push "$MOLOKO_IMAGE/api:$MOLOKO_SHA"
  deploy:
    needs: [image]
    agent: prod
    steps:
      - echo "$MOLOKO_REGISTRY_TOKEN" | docker login "$MOLOKO_REGISTRY" -u "$MOLOKO_REGISTRY_USER" --password-stdin
      - docker pull "$MOLOKO_IMAGE/api:$MOLOKO_SHA" && docker compose up -d
```

- `MOLOKO_IMAGE` is `<registry-host>/<project name, lowercased>`.
- **Auth.** Each run gets its own read-write token in `MOLOKO_REGISTRY_TOKEN`. The token stops working when the run ends. Agents can also pull outside a pipeline with their agent name and token, for example `docker login <host> -u web1 -p mlk_…`. Agent tokens can pull but not push. Requests without credentials get a 401.
- **Storage.** Content-addressed under `<data>/registry`. Layers stream to disk while being hashed, and cross-repo blob mounts and Range requests are supported. The **Images** page in the dashboard lists repositories and tags with their pull commands.
- **HTTPS.** Docker only talks plain HTTP to `localhost`. On the build machine, the default `localhost:<port>` works on Linux. For agents on other servers, and for Docker Desktop on macOS (its daemon runs in a VM), serve moloko over HTTPS and set `--registry-host` to that name. With Tailscale, run `tailscale serve --bg 8080` and set `MOLOKO_REGISTRY_HOST=<machine>.<tailnet>.ts.net`.
- Not yet included: garbage collection of untagged blobs, and retention policies.

## Deploy agents

Agents run on the servers you deploy to. Each one **dials out** to moloko over a WebSocket and keeps that connection open, so target servers need no inbound ports, SSH keys or firewall rules. Deploy instructions and logs travel over that connection.

1. In the dashboard, go to **Agents**, then **Add agent**. Give it a name and labels such as `prod, eu`, and copy the token. It is shown only once, and only its hash is stored.
2. On the target server, run the same binary in agent mode:
   ```sh
   MOLOKO_AGENT_TOKEN=mlk_… moloko agent --server wss://moloko.example.ts.net/api/agents/connect --workdir /srv/app
   ```
   The agent reconnects with backoff, and pings every 20s keep idle connections alive through proxies.
3. Target agents from a pipeline by label. The job runs on **every** connected agent that has all the listed labels, in parallel:
   ```yaml
   deploy:
     needs: [package]
     agent: prod            # or [prod, eu]
     steps:
       - docker compose pull && docker compose up -d
   ```
   The job fails if no matching agent is connected, or if an agent disconnects partway through. Agent jobs don't use a local executor slot.

**Reaching the server.** When moloko runs on a Mac mini behind NAT, agents still need a URL that reaches it:
- **Tailscale (recommended).** Install it on the build machine and on the targets, then use the MagicDNS name. The traffic is end-to-end WireGuard and is never exposed to the internet. `tailscale serve` adds HTTPS.
- **Cloudflare Tunnel.** Use it when you also need a public URL, for example for GitHub webhooks. Until moloko has auth, publish only `/api/agents/connect` and `/api/webhooks/github`, never the dashboard or the rest of the API.

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
| `GET/POST /api/agents`, `DELETE /api/agents/:id` | list (with online status) / register `{name, labels}` → `{token}` / revoke |
| `GET /api/agents/connect` | agent WebSocket (`Authorization: Bearer <token>`) |
| `GET /api/images` | registry repositories and tags |
| `/v2/…` | OCI Distribution API (Basic auth: run token or `<agent>:<agent token>`) |

## Dashboard development

```sh
cargo run -- serve                 # in server/
npm run dev                        # in dashboard/, proxies /api to :8080
```
