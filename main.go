package main

import (
	"context"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"runtime"
	"syscall"
	"time"
)

func main() {
	repoFlag         := flag.String("repo", "", "Git repository URL (required)")
	imageFlag        := flag.String("image", "", "Base image name (default: derived from repo name)")
	workersFlag      := flag.Int("workers", runtime.NumCPU(), "Number of parallel git clones")
	buildWorkersFlag := flag.Int("build-workers", 1, "Number of concurrent docker builds (default 1 to avoid overlay exhaustion)")
	pushFlag         := flag.Bool("push", false, "Push images to registry after building")
	dockerfileFlag   := flag.String("dockerfile", "Dockerfile", "Dockerfile path relative to repo root")
	keyFlag          := flag.String("key", "", "Path to SSH private key for repository access")
	portFlag         := flag.String("port", "", "Serve UI on this address, e.g. :8080")
	stateFlag        := flag.String("state", ".moloko-state.json", "Path to build-state file (tracks last-built SHAs)")
	watchFlag        := flag.Bool("watch", false, "Poll for branch changes and rebuild when commits arrive")
	intervalFlag     := flag.String("interval", "60s", "Polling interval for --watch mode")
	registryFlag     := flag.String("registry", "", "Start built-in OCI registry on this address, e.g. :5000 (implies --push)")
	registryDirFlag  := flag.String("registry-dir", ".moloko-registry", "Storage directory for the built-in registry")
	flag.Parse()

	if *repoFlag == "" {
		fmt.Fprintln(os.Stderr, "error: --repo is required")
		flag.Usage()
		os.Exit(1)
	}

	imageBase := *imageFlag
	if imageBase == "" {
		imageBase = repoBaseName(*repoFlag)
	}

	var gitSSHEnv string
	if *keyFlag != "" {
		if _, err := os.Stat(*keyFlag); err != nil {
			fmt.Fprintf(os.Stderr, "error: SSH key not found: %s\n", *keyFlag)
			os.Exit(1)
		}
		gitSSHEnv = sshEnv(*keyFlag)
	}

	var pollInterval time.Duration
	if *watchFlag {
		var err error
		pollInterval, err = time.ParseDuration(*intervalFlag)
		if err != nil {
			fmt.Fprintf(os.Stderr, "error: invalid --interval %q: %v\n", *intervalFlag, err)
			os.Exit(1)
		}
	}

	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()

	// Built-in registry: start server, prepend host to imageBase, enable push.
	var registryURL string
	var reg *Registry
	var builderName string
	if *registryFlag != "" {
		var err error
		reg, err = NewRegistry(*registryDirFlag)
		if err != nil {
			fmt.Fprintf(os.Stderr, "error: registry init: %v\n", err)
			os.Exit(1)
		}
		dockerHost := registryDockerHost()
		registryURL = dockerHost + *registryFlag
		imageBase = registryURL + "/" + imageBase
		*pushFlag = true

		// On macOS/Windows the Docker daemon lives in a VM. Docker's push
		// mechanism requires HTTPS for non-localhost registries, which we
		// can't easily satisfy. Instead we configure a dedicated buildx
		// builder whose buildkitd.toml marks our registry as plain-HTTP,
		// then use `docker buildx build --push` to combine build and push
		// in one step entirely inside Docker's VM.
		if err := setupBuildxBuilder(*registryDirFlag, registryURL); err != nil {
			fmt.Fprintf(os.Stderr, "error: buildx builder setup: %v\n", err)
			os.Exit(1)
		}
		builderName = buildxBuilderName

		// Configure the Docker daemon's insecure-registries so that plain
		// `docker pull` works. We write directly into the VM's daemon.json and
		// reload dockerd in one step — no Docker Desktop restart needed.
		fmt.Printf("Configuring Docker daemon for registry pull support… ")
		if err := configureDockerPull(registryURL); err != nil {
			fmt.Fprintf(os.Stderr, "\nwarning: daemon config failed: %v\n", err)
			fmt.Fprintf(os.Stderr, "To enable `docker pull`, add %q to insecure-registries in\nDocker Desktop → Settings → Docker Engine, then Apply & Restart.\n", registryURL)
		} else {
			fmt.Println("done.")
		}

		regSrv := &registryServer{reg: reg, addr: *registryFlag}
		go func() {
			fmt.Printf("Built-in registry listening on http://%s\n", registryURL)
			if err := regSrv.start(ctx); err != nil {
				fmt.Fprintf(os.Stderr, "registry error: %v\n", err)
				cancel()
			}
		}()
	}

	// Discover branches once upfront so the hub (and server) can be initialised
	// before accepting the first client connection.
	fmt.Printf("Querying branches for %s …\n", *repoFlag)
	branches, shas, err := discover(ctx, *repoFlag, gitSSHEnv)
	if err != nil {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
	if len(branches) == 0 {
		fmt.Println("No matching branches found (main/master or Jira ticket branches).")
		return
	}

	state := loadState(*stateFlag)
	state = reconcileState(ctx, branches, state, imageBase, reg)

	var hub *Hub
	if *portFlag != "" {
		hub = NewHub(*repoFlag, branches, registryURL)
		go func() {
			fmt.Printf("UI available at http://localhost%s\n", *portFlag)
			if err := startServer(ctx, *portFlag, hub); err != nil {
				fmt.Fprintf(os.Stderr, "server error: %v\n", err)
				cancel()
			}
		}()
	}

	// runCycle builds whichever branches have a new commit since the last build.
	runCycle := func(branches []string, shas map[string]string) {
		toBuild := changedBranches(branches, shas, state)
		if len(toBuild) == 0 {
			fmt.Println("All branches up to date.")
			return
		}

		if hub != nil {
			rebuildSet := make(map[string]bool, len(toBuild))
			for _, b := range toBuild {
				rebuildSet[b] = true
			}
			hub.ResetForCycle(branches, rebuildSet)
		} else {
			fmt.Printf("Building %d changed branch(es):\n", len(toBuild))
			for _, b := range toBuild {
				fmt.Printf("  %s\n", b)
			}
			fmt.Println()
		}

		overall := time.Now()
		results := buildAll(ctx, *repoFlag, toBuild, imageBase, *dockerfileFlag, gitSSHEnv, *pushFlag, *workersFlag, *buildWorkersFlag, hub, builderName)
		elapsed := time.Since(overall)

		var failed int
		for _, r := range results {
			if r.Err == nil {
				state.BuiltSHAs[r.Branch] = shas[r.Branch]
			} else {
				failed++
			}
		}
		if err := saveState(*stateFlag, state); err != nil {
			fmt.Fprintf(os.Stderr, "warning: could not save state: %v\n", err)
		}

		if hub != nil {
			hub.Finish(Summary{
				Succeeded: len(results) - failed,
				Failed:    failed,
				ElapsedMs: elapsed.Milliseconds(),
			})
		} else {
			fmt.Printf("\nDone in %v — %d succeeded, %d failed\n",
				elapsed.Round(time.Millisecond), len(results)-failed, failed)
			if failed > 0 {
				os.Exit(1)
			}
		}
	}

	runCycle(branches, shas)

	if !*watchFlag {
		if hub != nil {
			<-ctx.Done() // keep server alive until interrupted
		}
		return
	}

	// Watch loop: re-query every interval, rebuild whatever changed.
	for {
		select {
		case <-time.After(pollInterval):
		case <-ctx.Done():
			return
		}

		fmt.Printf("Checking for changes (%s) …\n", *repoFlag)
		branches, shas, err = discover(ctx, *repoFlag, gitSSHEnv)
		if err != nil {
			fmt.Fprintf(os.Stderr, "warning: branch discovery failed: %v\n", err)
			continue
		}
		runCycle(branches, shas)
	}
}

// discover fetches all remote branches and their SHAs, returning only those
// that match the build filter (main/master or Jira ticket branches).
func discover(ctx context.Context, repoURL, gitSSHEnv string) (branches []string, shas map[string]string, err error) {
	all, err := listBranches(ctx, repoURL, gitSSHEnv)
	if err != nil {
		return nil, nil, err
	}
	shas = make(map[string]string, len(all))
	for _, b := range all {
		if isRelevantBranch(b.Name) {
			branches = append(branches, b.Name)
			shas[b.Name] = b.SHA
		}
	}
	return branches, shas, nil
}

// changedBranches returns branches whose current SHA differs from the last built SHA.
func changedBranches(branches []string, shas map[string]string, state BuildState) []string {
	var changed []string
	for _, b := range branches {
		if state.BuiltSHAs[b] != shas[b] {
			changed = append(changed, b)
		}
	}
	return changed
}
