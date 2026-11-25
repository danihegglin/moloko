# moloko server

A self-hosted forge in one binary: CI pipelines, git hosting with GitHub mirroring, issues, reviews and AI agents that work in containers or VMs. It runs on a Mac mini or a Linux box and needs no JVM, plugins or external database.

```sh
make server                                # builds the dashboard and embeds it
./server/target/release/moloko serve       # http://127.0.0.1:8080
```

Open the dashboard and create the first admin account. Until that exists, anyone who can reach the server can claim it, so do this before you expose it.

| Flag / env | Default | |
|---|---|---|
| `--listen` / `MOLOKO_LISTEN` | `127.0.0.1:8080` | Create the admin account before listening on a public address. Put HTTPS in front (e.g. `tailscale serve`, Caddy) |
| `--data` / `MOLOKO_DATA` | `moloko-data` | SQLite DB, git mirrors, caches, logs, workspaces |
| `--executors` / `MOLOKO_EXECUTORS` | CPU count | Max jobs running at once |
| `--webhook-secret` / `MOLOKO_WEBHOOK_SECRET` | unset | Enables `POST /api/webhooks/github` (push events) |
| `--registry-host` / `MOLOKO_REGISTRY_HOST` | `localhost:<port>` | The address build jobs use for the built-in registry |
| `--wg-listen` / `MOLOKO_WG_LISTEN` | `0.0.0.0:51820` | UDP address of the built-in WireGuard endpoint for agents (`off` disables it) |
| `--wg-endpoint` / `MOLOKO_WG_ENDPOINT` | registry host + WireGuard port | `host:port` agents dial, written into join codes, e.g. your public IP or DNS name |
| `--mirror-interval` / `MOLOKO_MIRROR_INTERVAL` | `300` | Seconds between polls of mirrored GitHub repos. Webhooks make it immediate |
| `MOLOKO_SECRET_KEY` | `<data>/secret.key` | 32-byte hex key that encrypts mirror tokens and project secrets. Generated on first start if unset |

Requires `git` 2.38 or newer, `docker` for jobs that set `image:` and for container AI agents, and `tart` (macOS) or a [VM driver](../docs/vm-driver.md) for VM agents.

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

## Users and access

- **Accounts.** The first account is an admin. Admins add more users under **Account**. Passwords are hashed with Argon2, and the dashboard uses an HTTP-only session cookie.
- **Projects and roles.** Each project has members with `read`, `write` or `admin`. Whoever creates a project is its admin. Admins have admin rights everywhere.
- **Tokens.** Create personal access tokens (`mlkpat_…`) under **Account**. Use one as the password for git over HTTPS, or send it as `Authorization: Bearer` to the API.

## Projects, repositories and mirroring

A project holds issues, reviews, AI agents and one or more git repositories. Clone and push over HTTPS:

```sh
git clone https://moloko.example/git/<project>/<repo>.git
```

Each repository has a mirror mode, set when you add it or under **Settings**:

| Mode | Source of truth | Behaviour |
|---|---|---|
| `none` | moloko | Hosted here only. |
| `pull` | GitHub | Read-only copy. moloko fetches on the poll interval or on a webhook, and refuses pushes. |
| `push` | moloko | After moloko accepts a push, it pushes the same refs to GitHub with `--force-with-lease`. Failures are retried and shown per ref. |
| `twoway` | both | A push to moloko is written through to GitHub first, and is accepted only if GitHub accepts it (or queued, with `on_unreachable: queue`). Changes made on GitHub come back through the poll or webhook. When both sides moved, the ref is marked **diverged** and an admin picks which side to keep. The other side's commits stay at `refs/moloko/diverged/…`. |

Store a GitHub token on the repository for private repos and for pushing. Tokens are encrypted at rest. A project created without a GitHub URL starts with an empty hosted repository.

**Branch protection.** `protect_default` blocks force pushes and deletes on the default branch. `require_review` lets changes reach it only by merging a review.

**Webhooks.** Point a GitHub push webhook at `/api/webhooks/github` with `--webhook-secret`. It syncs the matching mirrored repositories at once.

## Multi-repo pipelines

A pipeline can check out sibling repositories of the same project next to its own, and can run when another repository's default branch passes:

```yaml
on:
  upstream: [api]                    # run after a green build of api's default branch
checkout:
  - repo: api                        # checked out at ../api
    ref: same-branch                 # the run's branch if api has it, else api's default branch
jobs:
  e2e:
    steps: [./scripts/e2e.sh ../api]
```

Jobs also get `MOLOKO_PROJECT`, `MOLOKO_REPO`, and `MOLOKO_REVIEW` when the run tests a review.

## Issues

Issues have a key per project (`SHOP-12`), a type, status (`backlog`, `todo`, `in_progress`, `in_review`, `done`, `cancelled`), priority, assignee, labels, estimate, parent, cycle and links (`blocks`, `relates`, `duplicates`). The **Issues** tab takes queries like `status:open assignee:me label:web type:bug login`. The **Board** tab is a drag-and-drop board. Cycles show a burndown. Import from a Jira CSV export or `gh issue list --json` output under **Settings**.

Git activity moves issues forward on its own and never moves them back:

- A branch named after an issue, like `shop-12-fix-login`, links to it and moves it to `in_progress`. **Create branch** on the issue makes one.
- Commits that mention `SHOP-12` are linked. Opening a review moves the issue to `in_review`.
- Merging a review that says `fixes SHOP-12` (or `closes`, `resolves`) closes the issue.
- A successful deploy job records where the change went. A deploy to `prod` or `production` marks the issue released.

## Reviews

Open a review from a branch in the **Reviews** tab, or follow the link `git push` prints. A review shows the diff with line comments, commits, checks and approvals. It can merge when nothing blocks it. The blockers are listed on the review.

Rules come from the `review:` section of `.moloko.yml` on the target branch:

```yaml
review:
  required_approvals: 1              # approvals count only for the current head commit
  required_jobs: [test]              # default: every job of the run must pass
  code_owners: true                  # each changed file needs an owner's approval (.moloko/CODEOWNERS)
  merge: squash                      # merge, squash or rebase
  merge_queue: true                  # test the merge result on top of the latest target before it lands
  ai_review: advisory                # off, advisory or required
  ai_reviewer: claude                # the AI agent that reviews
  human_approval_for_ai_changes: true
```

Authors cannot approve their own review. Unresolved threads and conflicts block merging.

**Change sets.** Reviews in different repositories of the same project that belong to the same issue form a change set. Merging one merges them all, and only if all of them can merge.

**Merge queue.** With `merge_queue: true`, **Merge** adds the review to a queue. moloko builds the merge result on top of the latest target at `refs/moloko/queue/<id>`, runs the pipeline on it, and lands it only if that passes.

## AI agents

An AI agent is a bot member of a project with an environment and a command. Set one up in the **AI agents** tab:

| Environment | Isolation |
|---|---|
| `container` | A Docker container from `image`, with optional `cpus` and `memory` limits. The workspace is mounted at `/workspace`. |
| `vm` | A fresh VM per session. Uses `tart` on macOS, or any driver program that follows the [VM driver protocol](../docs/vm-driver.md). |
| `host` | Runs directly on the build machine. Use only for trusted agents. |

The command is any CLI agent, for example `claude -p "$(cat "$MOLOKO_TASK_FILE")" --dangerously-skip-permissions`. Store its API key as a project secret and list it under the agent's `secrets`.

An agent starts a session when you:

- assign it an issue,
- mention it with `@name` in an issue or review comment,
- request changes on its review,
- name it as `ai_reviewer` (it then reviews every review), or
- turn on `auto_fix_ci` (it then tries to fix failing checks on its own reviews, at most 3 times).

Each session runs as a pipeline run with live logs. The checked-out issue branch is the working directory, with sibling repositories next to it. The session gets:

- `MOLOKO_TASK_FILE` with the issue, comments and instructions,
- `MOLOKO_BRANCH`, `MOLOKO_AGENT` and `MOLOKO_SESSION`,
- `MOLOKO_DIFF_FILE` and `MOLOKO_REVIEW_FILE` for reviews. The first line of the review file is `APPROVE` or `REQUEST_CHANGES`, followed by comments,
- `MOLOKO_TOKEN`, which can push only this branch and expires with the session,
- the registry variables and the listed secrets.

When the command finishes, moloko commits any leftover changes, pushes the branch and opens a review, or updates the existing one. Agents cannot merge, and by default their changes need a person's approval.

## How it stays fast

- **Git mirror plus worktrees.** Each project keeps one local mirror. A run does an incremental `git fetch`, and each job checks out with `git worktree add`, with no network clone per job.
- **Copy-on-write caches.** Cache entries are content-addressed by the `key_files` contents. They are restored with APFS `clonefile` on macOS and reflinks on btrfs/xfs, so a multi-GB cache restores almost instantly.
- **Parallel job graph.** Jobs start as soon as their `needs` pass, limited by `--executors`. When a dependency fails, the jobs downstream are skipped.
- **Warm containers.** A Docker job starts one container and runs each step with `docker exec`, instead of starting a container per step.
- **Live logs.** Logs stream over server-sent events, and the UI batches lines per animation frame.

## API

Everything except `/api/auth/*`, the webhook, agent connections and the registry needs a session cookie or a token.

| | |
|---|---|
| `POST /api/auth/setup`, `/login`, `/logout`, `GET /api/auth/me` | first admin / session cookie |
| `GET/POST /api/tokens`, `DELETE /api/tokens/:id` | personal access tokens |
| `GET/POST /api/users` | users (admin) |
| `GET/POST /api/projects` | list / create `{name, key?, repo_url?, mirror_mode?, credential?}` |
| `GET/PATCH /api/projects/:id` | project, repos, members and your role / change the issue key |
| `PUT /api/projects/:id/members`, `DELETE …/members/:user` | set a member's role / remove |
| `POST /api/projects/:id/repos`, `GET/PATCH/DELETE /api/repos/:id` | repositories and their mirror settings |
| `GET /api/repos/:id/mirror`, `POST …/sync`, `POST …/mirror/resolve` | mirror state per ref / sync now / resolve a diverged ref `{ref, keep: moloko\|upstream}` |
| `GET/POST /api/repos/:id/branches`, `GET …/tree`, `…/blob`, `…/commits`, `…/commits/:sha`, `…/compare` | browse code |
| `GET/POST /api/projects/:id/issues`, `GET …/issues/:number`, `GET/PATCH /api/issues/:id` | issues (`?q=` query) |
| `POST /api/issues/:id/comments`, `…/links`, `…/branch`, `…/ai` | comment / link / create branch / start an AI session |
| `GET/POST /api/projects/:id/cycles`, `GET /api/cycles/:id/burndown`, `POST /api/projects/:id/import` | cycles / burndown / import |
| `GET/POST /api/projects/:id/reviews`, `GET …/reviews/:number`, `GET/PATCH /api/reviews/:id` | reviews |
| `POST /api/reviews/:id/comments`, `…/comments/:cid/resolve`, `…/approve`, `…/merge`, `DELETE …/queue` | review actions |
| `GET/POST /api/projects/:id/ai-agents`, `PATCH/DELETE /api/ai-agents/:id`, `GET …/ai-sessions` | AI agents and sessions |
| `GET /api/projects/:id/secrets`, `PUT/DELETE …/secrets/:name` | project secrets (names only on read) |
| `/git/:project/:repo.git/…` | git smart HTTP (Basic auth: username and token) |
| `POST /api/projects/:id/runs` | trigger `{ref, repo_id?}` (default: the default branch) |
| `GET /api/runs?project_id=` | list runs |
| `GET /api/runs/:id` | run + jobs |
| `POST /api/runs/:id/cancel` | cancel (kills the step's whole process group) |
| `GET /api/jobs/:id/logs` | SSE stream: `line` events, then `end` |
| `POST /api/webhooks/github` | GitHub push webhook (HMAC-verified), syncs mirrored repos |
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
