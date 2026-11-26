# moloko runners

moloko runners are cloud build machines you rent by the minute, and they can't see what they build. A job marked `runner: cloud` runs on a single-use confidential VM (AMD SEV-SNP). Your own moloko server checks the VM's hardware attestation before it sends anything. Source, secrets and logs then travel over a channel whose keys only your server and that VM hold.

## Who does what

| Part | Runs where | Sees |
|---|---|---|
| `moloko serve` | Your machine | Everything. It holds the source and secrets, and verifies runners |
| `moloko broker serve` | moloko cloud | Your account, the size class, lease start and end, byte counts. It relays sealed frames it can't open |
| `moloko runner` | A confidential VM, one job then destroyed | The one job it was given, inside encrypted memory |

## Using it

```yaml
jobs:
  test:
    runner: cloud        # run on moloko runners instead of this machine
    size: l              # s | m | l | xl, default m
    shards: 16           # 16 runners in parallel
    image: eclipse-temurin:21
    fallback: local      # run here if no runner can be leased or verified
    steps:
      - ./scripts/test-shard.sh "$MOLOKO_SHARD_INDEX" "$MOLOKO_SHARD_TOTAL"
```

Start the server with your subscription:

| Flag / env | |
|---|---|
| `--cloud-key` / `MOLOKO_CLOUD_KEY` | Subscription key, `mlkcloud_…` |
| `--cloud-broker` / `MOLOKO_CLOUD_BROKER` | Broker URL |
| `--cloud-measurements` / `MOLOKO_CLOUD_MEASUREMENTS` | Comma-separated launch measurements (96 hex chars) of the runner images you accept |
| `--cloud-insecure-dev` / `MOLOKO_CLOUD_INSECURE_DEV` | Accept runners without hardware attestation. Development only: whoever runs such a runner can read your jobs |

Cloud jobs don't take a local executor slot. Without `fallback: local`, a job fails if no runner can be leased within a minute or if attestation fails. With it, the job runs locally instead. Either way nothing has been sent, because the job ships only after attestation passes.

The job page shows a **cloud** label with the size and the attestation result. **Account → Cloud runners** shows the m-minutes this server recorded this month, so you can check a bill against your own numbers. One `m` minute is one m-minute, an `l` minute is two, an `s` minute is half.

## How a job runs

1. The server opens `GET /v1/lease?size=…` on the broker with its key. The broker checks the plan's minutes and concurrency, takes an idle runner from the warm pool, and answers `leased`.
2. The server sends a fresh nonce and a per-lease X25519 public key.
3. The runner asks the CPU, through the kernel's configfs-tsm interface, for an SEV-SNP report whose user data is `SHA-512("moloko-runner-v1" ‖ runner key ‖ server key ‖ nonce)`. It returns the report, its VCEK certificate and its key.
4. The server verifies VCEK ← ASK ← ARK against AMD's root certificates built into the binary, the report signature (ECDSA P-384), that debugging and migration agents are off, that the launch measurement is on its allow-list, and that the user data matches. Any failure ends the lease.
5. Both sides derive ChaCha20-Poly1305 keys with HKDF-SHA256 from the shared secret, the nonce and the evidence. Frames carry a per-direction counter, so a replayed, dropped or reordered frame fails.
6. The server sends a `git bundle` of each repository and then the job (env, steps, image). The runner checks out, runs the steps, and streams log lines back.
7. When the job ends or either side disconnects, the runner exits and its VM is destroyed. The broker records the minutes.

## Running the cloud side

```sh
# Issue a key (plans: trial 500 m-minutes / 4 at a time, team 3,000 / 20, business 40,000 / 100)
moloko broker add-key --data /srv/broker --account acme --plan team

# Serve the broker
MOLOKO_POOL_TOKEN=… moloko broker serve --listen 0.0.0.0:8090 --data /srv/broker

# Inside each runner VM, as its only service
MOLOKO_POOL_TOKEN=… moloko runner --broker https://runners.example.com --size m --workdir /scratch
```

The runner fetches its VCEK from AMD's key distribution service at boot (`--amd-product Milan|Genoa`), or reads it from `--vcek`. `moloko runner --dev` skips attestation for local testing, and servers refuse it unless they run with `--cloud-insecure-dev`.

## Not built yet

- **Provisioning.** Nothing boots or destroys VMs yet. The broker expects runners to join `/v1/pool` on their own. A provisioner for AWS SEV-SNP instances and a reproducible, measured runner image (with an encrypted scratch disk) come next.
- **Intel TDX** evidence and Turin VCEK fetching. Turin certificates are built in, but the runner fetches VCEKs only for Milan and Genoa.
- **Minimum TCB policy.** The reported TCB is recorded on the job, but not checked against a minimum.
- **Tunnels over the channel:** the built-in registry, caches and `egress: home`. Cloud jobs can't use `cache:` yet, and `MOLOKO_REGISTRY` still points at your server, which a runner can't reach.
- **Billing.** The broker meters minutes and enforces plan limits. Stripe and sign-up are not wired up.
- **Settings in the dashboard.** The key and measurements come from flags or env, and the dashboard shows them read-only.
