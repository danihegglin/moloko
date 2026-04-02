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
	repoFlag := flag.String("repo", "", "Git repository URL (required)")
	imageFlag := flag.String("image", "", "Base image name (default: derived from repo name)")
	workersFlag := flag.Int("workers", runtime.NumCPU(), "Number of parallel git clones")
	buildWorkersFlag := flag.Int("build-workers", 1, "Number of concurrent docker builds (default 1 to avoid overlay exhaustion)")
	pushFlag := flag.Bool("push", false, "Push images to registry after building")
	dockerfileFlag := flag.String("dockerfile", "Dockerfile", "Dockerfile path relative to repo root")
	keyFlag := flag.String("key", "", "Path to SSH private key for repository access")
	portFlag := flag.String("port", "", "Serve UI on this address, e.g. :8080")
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

	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()

	fmt.Printf("Querying branches for %s …\n", *repoFlag)
	allBranches, err := listBranches(ctx, *repoFlag, gitSSHEnv)
	if err != nil {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}

	var matched []string
	for _, b := range allBranches {
		if isRelevantBranch(b) {
			matched = append(matched, b)
		}
	}

	if len(matched) == 0 {
		fmt.Println("No matching branches found (main/master or Jira ticket branches).")
		return
	}

	if *portFlag != "" {
		hub := NewHub(*repoFlag, matched)

		go func() {
			overall := time.Now()
			results := buildAll(ctx, *repoFlag, matched, imageBase, *dockerfileFlag, gitSSHEnv, *pushFlag, *workersFlag, *buildWorkersFlag, hub)
			elapsed := time.Since(overall)

			var failed int
			for _, r := range results {
				if r.Err != nil {
					failed++
				}
			}
			hub.Finish(Summary{
				Succeeded: len(results) - failed,
				Failed:    failed,
				ElapsedMs: elapsed.Milliseconds(),
			})
		}()

		fmt.Printf("UI available at http://%s\n", *portFlag)
		if err := startServer(ctx, *portFlag, hub); err != nil {
			fmt.Fprintf(os.Stderr, "server error: %v\n", err)
			os.Exit(1)
		}
		return
	}

	fmt.Printf("Building %d branch(es): %d parallel clone(s), %d concurrent docker build(s):\n",
		len(matched), *workersFlag, *buildWorkersFlag)
	for _, b := range matched {
		fmt.Printf("  %s\n", b)
	}
	fmt.Println()

	overall := time.Now()
	results := buildAll(ctx, *repoFlag, matched, imageBase, *dockerfileFlag, gitSSHEnv, *pushFlag, *workersFlag, *buildWorkersFlag, nil)
	elapsed := time.Since(overall)

	var failed int
	for _, r := range results {
		if r.Err != nil {
			failed++
		}
	}

	fmt.Printf("\nDone in %v — %d succeeded, %d failed\n",
		elapsed.Round(time.Millisecond), len(results)-failed, failed)
	if failed > 0 {
		os.Exit(1)
	}
}
