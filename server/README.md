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
| `--poll-interval` / `MOLOKO_POLL_INTERVAL` | `60` | Seconds between checks for new commits in repos with **Build on push** enabled; `0` disables polling |
| `--registry-host` / `MOLOKO_REGISTRY_HOST` | `localhost:<port>` | The address build jobs use for the built-in registry |
| `--wg-listen` / `MOLOKO_WG_LISTEN` | `0.0.0.0:51820` | UDP address of the built-in WireGuard endpoint for agents (`off` disables it) |
| `--wg-endpoint` / `MOLOKO_WG_ENDPOINT` | registry host + WireGuard port | `host:port` agents dial, written into join codes, e.g. your public IP or DNS name |
| `--gha-image` / `MOLOKO_GHA_IMAGES` | see below | Runner image for a GitHub workflow `runs-on` label: `label=image`, repeatable or comma-separated |
| `--github-token` / `MOLOKO_GITHUB_TOKEN` | unset | Fallback `GITHUB_TOKEN` for workflows when no project or repo secret sets one, also used to fetch private actions |
| `MOLOKO_SECRETS_KEY` | `<data>/secrets.key` | Base64 32-byte key that encrypts the secret vault; generated on first start if unset |

Requires `git`, and `docker` for jobs that set `image:` and for GitHub workflows.

## Projects and repos

A project groups git repos. Add them under **Projects** in the dashboard. Each repo has its own `.moloko.yml`, git mirror and runs, and GitHub push webhooks are matched to repos by URL. You can edit a repo's URL or delete repos and projects, but not while one of their runs is active. Changing the URL drops the mirror, so the next run clones again. For private repos, use an SSH URL (`git@github.com:owner/repo.git`): git runs without a terminal, so it can't prompt for an HTTPS password.

**Build on push.** Runs start from the dashboard, from a GitHub webhook, or by polling. Polling needs no public address: enable **Build on push** on a repo and moloko runs `git ls-remote` against it every `--poll-interval` seconds. A branch or tag that moved, or a new one, starts the same run a push webhook would. The first check after enabling only records where the branches are, so nothing that already exists is built. A push that no pipeline listens for leaves no run. With both a webhook and polling, a push still starts one run.

## Secrets

Secrets live at three levels. Server secrets, under **Settings**, reach every repository. Project and repository secrets are under **Projects**, in a project's or repository's **⋯ → Secrets**. With the same name, a project secret overrides a server secret, and a repository secret overrides both. Names are uppercase letters, digits and `_`.

- **Storage.** Values are encrypted with ChaCha20-Poly1305 and bound to their scope and name. The key is `<data>/secrets.key` (mode 0600), or `MOLOKO_SECRETS_KEY`. Back it up: without it the stored secrets can't be decrypted. The API is write-only. It lists names and update times, and you can overwrite or delete a secret, but never read a value back.
- **Workflows** read secrets as `${{ secrets.NAME }}`. A `GITHUB_TOKEN` secret also becomes `github.token`, which actions like tauri-action or setup-node use. Unset secrets are empty, as on GitHub.
- **`.moloko.yml` jobs** list what they need: `secrets: [NPM_TOKEN]` exposes it as an environment variable. A missing secret fails the job.
- **Logs.** Every secret value (and each line of a multi-line value) is replaced with `***`. Values reach `docker exec`/`docker run` through the environment, not the command line, so they don't show up in process lists on the build machine.

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

## GitHub Actions workflows

moloko also runs the workflows in `.github/workflows/`, without changes. Every workflow whose `on:` matches becomes its own run, next to the `.moloko.yml` run if the repo has one.

- **Triggers.** A push (webhook or polling) runs workflows with `on: push`. Branch, tag and path filters (`branches`, `tags`, `paths`, and their `-ignore` forms) are honoured. A manual run from the dashboard or API runs workflows with `on: workflow_dispatch`. The Run dialog lets you pick a single workflow and fill in its `inputs`. The API takes `{"ref", "workflow": "release.yml", "inputs": {...}}`, and required inputs are checked before the run starts. A workflow file that fails to parse still gets a failed run that shows the error.
- **Runners.** Each job runs in a container from the image mapped to its `runs-on` label. By default `ubuntu-latest`, `ubuntu-24.04`, `ubuntu-22.04` and `ubuntu-20.04` map to `moloko/ubuntu:*`. Those are the `catthehacker/ubuntu:act-*` images that [act](https://github.com/nektos/act) uses, plus packages GitHub's runners have and those images lack (`xdg-utils`). moloko builds them on first use. Other labels fall back to `ubuntu-latest`. Map labels with e.g. `--gha-image ubuntu-latest=catthehacker/ubuntu:full-latest,self-hosted=my/runner`. The image `host` runs a job directly on the build machine. `container:` replaces the runner image. The Docker socket is mounted, so `docker build` works in steps.
- **Workspaces on macOS.** On a Mac, container jobs keep their workspace in a Docker volume (`moloko-ws-<job>`), which is native Linux disk inside Docker's VM. A macOS folder shared into the VM would be case-insensitive, which breaks Linux tooling like AppImage bundling, and it drops files under heavy parallel writes (`npm ci`). Checkout, `hashFiles()` and local actions work inside the container. The volume is removed when the job ends. Container jobs also get `APPIMAGE_EXTRACT_AND_RUN=1`, because containers have no FUSE.
- **macOS.** macOS can't run in a container, so when moloko runs on a Mac, `macos-*` jobs run on the host, the way GitHub's macOS runners do. Steps run as your user, but with moloko's own persistent `HOME` and tool cache under `<data>/gha/host`. JavaScript actions use the `node` on the host's `PATH`. `container:` and `services:` aren't available for host jobs, just as on GitHub's macOS runners. On a Linux server, `macos-*` jobs fail with an explanation.
- **Windows.** Windows can't run in a container on a Mac or Linux host, so `windows-*` jobs run on Ubuntu with a Windows cross toolchain: Rust with the `x86_64-pc-windows-msvc` target, [cargo-xwin](https://github.com/rust-cross/cargo-xwin), clang-cl, lld and NSIS. moloko builds this image (`moloko/windows-cross:2`, from [`runners/windows-cross.Dockerfile`](runners/windows-cross.Dockerfile)) the first time a Windows job runs. That takes a few minutes. cargo-xwin's MSVC CRT and Windows SDK download (about 2 GB, also on first use) is cached under `<data>/gha/xwin-cache`. Both, and the Ubuntu runner images, can be downloaded ahead of time under **Settings** in the dashboard. `runner.os` is `Linux` in these jobs, so a workflow can choose cross-compile flags only when it runs in moloko:
  ```yaml
  # Tauri: build the Windows NSIS installer from Linux
  args: ${{ matrix.args }}${{ matrix.os == 'windows-latest' && runner.os == 'Linux' && ' --runner cargo-xwin --target x86_64-pc-windows-msvc' || '' }}
  ```
  This produces unsigned `.exe`/NSIS builds. MSI installers and steps that need a real Windows machine (PowerShell scripts, signing with signtool) don't work.
- **Jobs.** `needs`, `if` (with `success()`, `failure()`, `always()`, `cancelled()`), `strategy.matrix` (with `include` and `exclude`, and matrices built with `fromJSON`), `fail-fast`, `max-parallel`, `outputs`, `env`, `defaults.run`, `timeout-minutes` and `continue-on-error` all work. `services:` containers share the job's network, so both `localhost:<port>` and `<service>:<port>` reach them.
- **Steps.** `run:` with `shell` (bash, sh, python, pwsh, or a custom `cmd {0}`), the full `${{ }}` expression language, `$GITHUB_ENV`, `$GITHUB_OUTPUT`, `$GITHUB_PATH`, `$GITHUB_STATE`, and workflow commands (`::add-mask::`, `::group::`, `::error::`, …).
- **Actions.** `uses: owner/repo@ref` is fetched once from GitHub and cached under `<data>/gha/actions`. It runs as a JavaScript action (`node*`, including `pre` and `post`), a Docker action (`Dockerfile` or `docker://`), or a composite action. Local actions (`./path`) and `docker://image` steps work too.
- **Built in.** `actions/checkout` checks out from moloko's git mirror, with no network or token needed. `actions/cache` (with `restore-keys`, `/restore` and `/save`) and `actions/upload-artifact`/`download-artifact` (shared between the jobs of a run) are implemented natively, because the real ones need GitHub's hosted services.
- **Not yet supported:** reusable workflows (`jobs.<id>.uses`), `pull_request` and schedule triggers, repository secrets and `vars` (only `GITHUB_TOKEN` is set), cache path globs, artifact exclude patterns, and downloading artifacts from the dashboard. Without `--github-token`, `github.token` is empty. Actions that only use it for rate limits (e.g. setup-node) still work, and moloko logs a hint when an action input falls back to it.

Check a workflow locally with `moloko validate .github/workflows/ci.yml`.

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

- `MOLOKO_IMAGE` is `<registry-host>/<project>/<repo>`, lowercased.
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
| `GET/POST /api/projects` | list (with their repos) / create `{name}` |
| `PATCH/DELETE /api/projects/:id` | rename `{name}` / delete with all repos and runs |
| `POST /api/projects/:id/repos` | add a repo `{url, name?}` (name defaults to the URL's last segment) |
| `PATCH/DELETE /api/repos/:id` | change `{name?, url?}` / delete with its runs |
| `POST /api/repos/:id/runs` | trigger `{ref}` (default `HEAD`) |
| `GET /api/runs?project_id=&repo_id=` | list runs, each with its jobs |
| `GET /api/runs/:id` | run + jobs (running jobs include the current `step`), and `group`: all runs the same trigger started |
| `GET /api/secrets`, `PUT`/`DELETE /api/secrets/:name` | server-wide secrets (names only; values are write-only). The same under `/api/projects/:id` and `/api/repos/:id` |
| `GET /api/downloads` | runner images and the Windows SDK, with whether each is downloaded |
| `POST /api/downloads` | `{"id": "..."}` — download one in the background |
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
