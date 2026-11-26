# VM driver protocol

AI agents with `environment.kind: vm` get a fresh virtual machine for every session. On macOS, moloko uses [tart](https://tart.run) when no driver is set. Anywhere else, set `environment.driver` to the path of a driver program. A driver is any executable, often a short shell script around Firecracker, Lima, libvirt, Proxmox or a cloud API.

moloko calls the driver with three commands. Arguments are passed as separate words. A non-zero exit means failure.

## `create <name> <image> <cpus> <memory_mb> <workspace>`

Start a VM called `<name>` from `<image>` (the agent's `environment.image`), with `<cpus>` CPUs and `<memory_mb>` MiB of memory. Mount the host directory `<workspace>` inside the VM at `/workspace`, read-write. Return only when commands can run in the VM.

The workspace holds the checked-out repositories (`/workspace/<repo>`) and the task files the session's environment variables point to.

## `exec <name> <script>`

Run `<script>` with `sh -e -c` inside the VM and stream its stdout and stderr. Exit with the script's exit code. The script starts by exporting the session's environment variables, including secrets and a short-lived push token, then changes to the repository directory. Don't log the script itself.

moloko enforces the agent's timeout and handles cancellation by killing the driver's process group, so `exec` should stop the command inside the VM when it receives SIGTERM.

## `destroy <name>`

Stop and delete the VM. moloko calls this after every session, including failed and cancelled ones. It must succeed if the VM is already gone.

## Example

A sketch of a driver around Lima on Linux. Check the flags against your Lima version:

```sh
#!/bin/sh
set -e
case "$1" in
  create)  limactl create --name="$2" --cpus="$4" --memory="$(( $5 / 1024 ))" \
             --mount="$6:w" --set=".mounts[0].mountPoint=\"/workspace\"" "$3" --tty=false
           limactl start "$2" ;;
  exec)    limactl shell "$2" sh -e -c "$3" ;;
  destroy) limactl delete --force "$2" || true ;;
  *)       echo "usage: $0 create|exec|destroy ..." >&2; exit 2 ;;
esac
```

The VM image needs `git`, `sh` and whatever tools the agent's command uses.
