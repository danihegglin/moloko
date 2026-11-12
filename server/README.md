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
| `--registry-host` / `MOLOKO_REGISTRY_HOST` | `localhost:<port>` | The address build jobs use for the built-in registry |
| `--wg-listen` / `MOLOKO_WG_LISTEN` | `0.0.0.0:51820` | UDP address of the built-in WireGuard endpoint for agents (`off` disables it) |
| `--wg-endpoint` / `MOLOKO_WG_ENDPOINT` | registry host + WireGuard port | `host:port` agents dial, written into join codes, e.g. your public IP or DNS name |

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
- **HTTPS.** Docker only talks plain HTTP to `localhost`. Agents connected over WireGuard pull from their own `localhost:5000`, so they need nothing extra. On the build machine, the default `localhost:<port>` works on Linux. Docker Desktop on macOS runs its daemon in a VM, so it needs moloko served over HTTPS, for example with `tailscale serve`, with `--registry-host` set to that name.
- Not yet included: garbage collection of untagged blobs, and retention policies.

## Deploy agents

Agents run on the servers you deploy to. They connect to moloko over **built-in WireGuard**: the server listens on one UDP port, and each agent has its own key pair. Deploy instructions, logs and image pulls all travel inside the tunnel. Target servers need no inbound ports, SSH keys, certificates or VPN client. Both sides run WireGuard in userspace (boringtun plus a userspace TCP/IP stack), so they need no root and no TUN device.

1. In the dashboard, go to **Agents**, then **Add agent**. Give it a name and labels such as `prod, eu`, and copy the join code. The code holds the agent's private key, the server's public key, its tunnel IP and its token. It is shown only once. **New join code** rotates the key and token and disconnects the old agent.
2. On the target server, run the same binary in agent mode:
   ```sh
   MOLOKO_JOIN=mlkjoin_… moloko agent --workdir /srv/app
   ```
   The agent sends a WireGuard keepalive every 25s to hold its NAT mapping open, and it reconnects with backoff. Use `--endpoint host:port` to override the server address in the code.
3. Target agents from a pipeline by label. The job runs on **every** connected agent that has all the listed labels, in parallel:
   ```yaml
   deploy:
     needs: [package]
     agent: prod            # or [prod, eu]
     steps:
       - echo "$MOLOKO_REGISTRY_TOKEN" | docker login "$MOLOKO_REGISTRY" -u moloko --password-stdin
       - docker pull "$MOLOKO_IMAGE/api:$MOLOKO_SHA" && docker compose up -d
   ```
   The job fails if no matching agent is connected, or if an agent disconnects partway through. Agent jobs don't use a local executor slot.

**Registry on agents.** Each agent serves the build server's registry on `localhost:5000`, tunnelled through WireGuard. You can change the port with `--registry-port`. Docker allows plain HTTP to `localhost`, so pulls work with no TLS setup. In agent jobs, `MOLOKO_REGISTRY` and `MOLOKO_IMAGE` already point there.

**Reachability.** Agents must be able to reach the server's UDP port. A Mac mini behind a home or office router needs that port (51820 by default) forwarded to it. Set `--wg-endpoint` to your public IP or a DNS name. The server never connects out to agents, so the agents themselves can sit behind any NAT.

Without WireGuard, `moloko agent --server wss://…/api/agents/connect --token mlk_…` still connects over a WebSocket. Use that when the server sits behind something like Tailscale or Cloudflare Tunnel.

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
| `GET/POST /api/agents`, `DELETE /api/agents/:id` | list (with online status) / register `{name, labels}` → `{token, join}` / revoke |
| `POST /api/agents/:id/join` | rotate the agent's WireGuard key and token, returns a new join code |
| `GET /api/agents/connect` | agent WebSocket (`Authorization: Bearer <token>`) |
| `GET /api/images` | registry repositories and tags |
| `/v2/…` | OCI Distribution API (Basic auth: run token or `<agent>:<agent token>`) |

## Dashboard development

```sh
cargo run -- serve                 # in server/
npm run dev                        # in dashboard/, proxies /api to :8080
```
