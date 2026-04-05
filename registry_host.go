package main

import (
	"bytes"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"time"
)

// buildxBuilderName is the name of the Docker buildx builder moloko creates
// and manages for pushing to the built-in registry.
const buildxBuilderName = "moloko-registry"

// registryDockerHost returns the hostname the Docker daemon should use to reach
// the built-in registry. On Linux, Docker runs natively and can reach localhost.
// On macOS and Windows, Docker Desktop runs inside a VM where localhost refers
// to the VM itself, not the host. host.docker.internal is the DNS name Docker
// Desktop provides so containers (including buildkitd) can reach host ports.
func registryDockerHost() string {
	if runtime.GOOS == "darwin" || runtime.GOOS == "windows" {
		return "host.docker.internal"
	}
	return "localhost"
}

// setupBuildxBuilder creates (or reuses) a Docker buildx builder configured to
// push to the built-in registry over plain HTTP. This sidesteps Docker's TLS
// enforcement for non-localhost registries: BuildKit runs inside Docker
// Desktop's VM, where host.docker.internal is reachable, and buildkitd.toml
// tells it to use HTTP for our registry address.
//
// The builder is recreated whenever the registry address changes (detected via
// a small sentinel file). The user can also force recreation by running
// `docker buildx rm moloko-registry`.
func setupBuildxBuilder(dir, registryAddr string) error {
	tomlPath := filepath.Join(dir, "buildkitd.toml")
	tomlContent := fmt.Sprintf("[registry.%q]\n  http = true\n  insecure = true\n", registryAddr)
	if err := os.WriteFile(tomlPath, []byte(tomlContent), 0o644); err != nil {
		return fmt.Errorf("write buildkitd.toml: %w", err)
	}

	// Reuse the existing builder if it was created for the same registry address.
	addrFile := filepath.Join(dir, "builder-addr")
	storedAddr, _ := os.ReadFile(addrFile)
	builderUp := exec.Command("docker", "buildx", "inspect", buildxBuilderName).Run() == nil
	if builderUp && string(storedAddr) == registryAddr {
		return nil
	}

	// Remove the stale builder if it exists.
	if builderUp {
		_ = exec.Command("docker", "buildx", "rm", buildxBuilderName).Run()
	}

	fmt.Printf("Creating buildx builder %q for registry %s …\n", buildxBuilderName, registryAddr)
	var stderr bytes.Buffer
	cmd := exec.Command("docker", "buildx", "create",
		"--name", buildxBuilderName,
		"--driver", "docker-container",
		"--config", tomlPath,
		"--bootstrap",
	)
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("docker buildx create: %w: %s", err, bytes.TrimSpace(stderr.Bytes()))
	}

	return os.WriteFile(addrFile, []byte(registryAddr), 0o644)
}

// configureDockerPull makes addr accessible via plain `docker pull` over HTTP.
//
// On macOS/Windows, Docker Desktop's daemon runs inside a Linux VM and reads
// its config from /etc/docker/daemon.json *inside* that VM — not from
// ~/.docker/daemon.json on the host. Writing to the host file and sending
// SIGHUP has no effect until Docker Desktop is restarted and syncs the files.
//
// Instead we:
//  1. Read the VM's active daemon.json through /proc/1/root (the VM's root
//     filesystem, reachable from a privileged container with --pid=host).
//  2. Merge addr into insecure-registries in Go.
//  3. Write the merged config back into the VM via the same mechanism.
//  4. Send SIGHUP to dockerd so it reloads without a restart.
//  5. Also persist to ~/.docker/daemon.json so the setting survives restarts.
func configureDockerPull(addr string) error {
	// Read the daemon's live config from inside the VM.
	existing, _ := exec.Command("docker", "run", "--rm", "--privileged", "--pid=host",
		"busybox", "cat", "/proc/1/root/etc/docker/daemon.json",
	).Output()

	cfg := make(map[string]json.RawMessage)
	if len(existing) > 0 {
		_ = json.Unmarshal(existing, &cfg)
	}

	var regs []string
	if raw, ok := cfg["insecure-registries"]; ok {
		_ = json.Unmarshal(raw, &regs)
	}
	for _, r := range regs {
		if r == addr {
			persistHostDaemonJSON(addr) // keep host copy in sync
			return nil                  // already active in the VM
		}
	}

	regs = append(regs, addr)
	raw, _ := json.Marshal(regs)
	cfg["insecure-registries"] = raw
	newJSON, _ := json.MarshalIndent(cfg, "", "  ")

	// Encode as base64 so the JSON can be embedded safely in a shell one-liner.
	b64 := base64.StdEncoding.EncodeToString(newJSON)

	// Write the new config into the VM and signal dockerd to reload — one
	// container invocation so we only pay the startup cost once.
	// The loop finds dockerd by reading /proc/*/comm (no pgrep needed).
	script := fmt.Sprintf(
		`printf '%%s' '%s' | base64 -d > /proc/1/root/etc/docker/daemon.json`+
			` && for d in /proc/[0-9]*/; do`+
			`   [ "$(cat "${d}comm" 2>/dev/null)" = "dockerd" ] && kill -HUP "$(basename "$d")" && break;`+
			` done`,
		b64,
	)

	var stderr bytes.Buffer
	cmd := exec.Command("docker", "run", "--rm", "--privileged", "--pid=host",
		"busybox", "sh", "-c", script,
	)
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("%w: %s", err, bytes.TrimSpace(stderr.Bytes()))
	}
	time.Sleep(2 * time.Second) // allow dockerd to finish reloading

	persistHostDaemonJSON(addr)
	return nil
}

// persistHostDaemonJSON adds addr to ~/.docker/daemon.json on the host so the
// insecure-registries setting survives Docker Desktop restarts. Errors here are
// non-fatal — the VM config is already updated.
func persistHostDaemonJSON(addr string) {
	home, err := os.UserHomeDir()
	if err != nil {
		return
	}
	path := filepath.Join(home, ".docker", "daemon.json")

	cfg := make(map[string]json.RawMessage)
	if data, err := os.ReadFile(path); err == nil {
		_ = json.Unmarshal(data, &cfg)
	}

	var regs []string
	if raw, ok := cfg["insecure-registries"]; ok {
		_ = json.Unmarshal(raw, &regs)
	}
	for _, r := range regs {
		if r == addr {
			return
		}
	}

	regs = append(regs, addr)
	raw, _ := json.Marshal(regs)
	cfg["insecure-registries"] = raw
	data, _ := json.MarshalIndent(cfg, "", "  ")
	_ = os.MkdirAll(filepath.Dir(path), 0o755)
	_ = atomicWriteFile(path, data)
}
