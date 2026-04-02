package main

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"time"
)

var (
	// Matches Jira-style ticket IDs anywhere in the branch name, e.g. PROJ-123, ABC-4567
	jiraRe = regexp.MustCompile(`(?i)\b[A-Z]{2,10}-\d+\b`)

	tagReplacer = strings.NewReplacer(
		"/", "-",
		" ", "-",
		"#", "",
		"_", "-",
	)
)

// isRelevantBranch reports whether a branch should have an image built.
func isRelevantBranch(name string) bool {
	if name == "main" || name == "master" {
		return true
	}
	return jiraRe.MatchString(name)
}

// branchToTag converts a branch name to a valid Docker image tag.
func branchToTag(branch string) string {
	tag := strings.ToLower(tagReplacer.Replace(branch))
	return strings.Trim(tag, "-")
}

// repoBaseName derives an image base name from a repository URL.
func repoBaseName(url string) string {
	url = strings.TrimSuffix(url, ".git")
	parts := strings.Split(url, "/")
	if len(parts) > 0 {
		return strings.ToLower(parts[len(parts)-1])
	}
	return "image"
}

// sshEnv builds a GIT_SSH_COMMAND environment entry for the given key file.
// StrictHostKeyChecking=accept-new trusts new hosts automatically (avoids
// interactive prompts) while still detecting changed host keys.
func sshEnv(keyPath string) string {
	return fmt.Sprintf(
		"GIT_SSH_COMMAND=ssh -i %s -o StrictHostKeyChecking=accept-new -o BatchMode=yes",
		keyPath,
	)
}

// listBranches queries the remote repository without cloning it.
func listBranches(ctx context.Context, repoURL, gitSSHEnv string) ([]string, error) {
	var stderr bytes.Buffer
	cmd := exec.CommandContext(ctx, "git", "ls-remote", "--heads", repoURL)
	cmd.Stderr = &stderr
	if gitSSHEnv != "" {
		cmd.Env = append(os.Environ(), gitSSHEnv)
	}
	out, err := cmd.Output()
	if err != nil {
		msg := strings.TrimSpace(stderr.String())
		if msg != "" {
			return nil, fmt.Errorf("git ls-remote: %w\n%s", err, msg)
		}
		return nil, fmt.Errorf("git ls-remote: %w", err)
	}
	var branches []string
	for _, line := range strings.Split(strings.TrimSpace(string(out)), "\n") {
		if line == "" {
			continue
		}
		// Each line: <hash>\trefs/heads/<branch>
		parts := strings.SplitN(line, "\t", 2)
		if len(parts) != 2 {
			continue
		}
		branches = append(branches, strings.TrimPrefix(parts[1], "refs/heads/"))
	}
	return branches, nil
}

// BuildResult holds the outcome of a single image build.
type BuildResult struct {
	Branch  string
	Image   string
	Elapsed time.Duration
	Err     error
}

// buildBranch shallow-clones a branch, then acquires the docker semaphore before
// running docker build + prune so that at most cap(dockerSem) builds run at once.
func buildBranch(ctx context.Context, repoURL, branch, imageBase, dockerfile, gitSSHEnv string, push bool, dockerSem chan struct{}, hub *Hub) BuildResult {
	start := time.Now()
	image := imageBase + ":" + branchToTag(branch)
	res := BuildResult{Branch: branch, Image: image}

	if hub != nil {
		hub.Update(BranchState{Branch: branch, Phase: PhaseCloning, Image: image})
	}

	tmpDir, err := os.MkdirTemp("", "moloko-")
	if err != nil {
		res.Err = fmt.Errorf("mktemp: %w", err)
		res.Elapsed = time.Since(start)
		if hub != nil {
			hub.Update(BranchState{Branch: branch, Phase: PhaseFailed, Image: image, ElapsedMs: res.Elapsed.Milliseconds(), Error: res.Err.Error()})
		}
		return res
	}
	defer os.RemoveAll(tmpDir)

	cloneDir := filepath.Join(tmpDir, "src")

	// Git clones run in parallel — they are I/O bound and don't touch Docker storage.
	if err := run(ctx, tmpDir, gitSSHEnv, "git",
		"clone", "--depth=1", "--branch", branch, "--single-branch",
		repoURL, cloneDir,
	); err != nil {
		res.Err = fmt.Errorf("git clone: %w", err)
		res.Elapsed = time.Since(start)
		if hub != nil {
			hub.Update(BranchState{Branch: branch, Phase: PhaseFailed, Image: image, ElapsedMs: res.Elapsed.Milliseconds(), Error: res.Err.Error()})
		}
		return res
	}

	if hub != nil {
		hub.Update(BranchState{Branch: branch, Phase: PhaseBuilding, Image: image, ElapsedMs: time.Since(start).Milliseconds()})
	}

	// Acquire the docker semaphore: limits how many builds touch Docker's overlay
	// storage simultaneously, preventing "no space left on device" under parallel load.
	dockerSem <- struct{}{}
	buildErr := run(ctx, cloneDir, "", "docker",
		"build", "-t", image,
		"-f", filepath.Join(cloneDir, dockerfile),
		cloneDir,
	)
	// Prune dangling build cache before releasing the slot so the next build
	// starts with freed space. Active layers are pinned and won't be removed.
	_ = run(ctx, "", "", "docker", "builder", "prune", "-f")
	<-dockerSem

	if buildErr != nil {
		res.Err = fmt.Errorf("docker build: %w", buildErr)
		res.Elapsed = time.Since(start)
		if hub != nil {
			hub.Update(BranchState{Branch: branch, Phase: PhaseFailed, Image: image, ElapsedMs: res.Elapsed.Milliseconds(), Error: res.Err.Error()})
		}
		return res
	}

	if push {
		if hub != nil {
			hub.Update(BranchState{Branch: branch, Phase: PhasePushing, Image: image, ElapsedMs: time.Since(start).Milliseconds()})
		}
		if err := run(ctx, cloneDir, "", "docker", "push", image); err != nil {
			res.Err = fmt.Errorf("docker push: %w", err)
			res.Elapsed = time.Since(start)
			if hub != nil {
				hub.Update(BranchState{Branch: branch, Phase: PhaseFailed, Image: image, ElapsedMs: res.Elapsed.Milliseconds(), Error: res.Err.Error()})
			}
			return res
		}
	}

	res.Elapsed = time.Since(start)
	if hub != nil {
		hub.Update(BranchState{Branch: branch, Phase: PhaseDone, Image: image, ElapsedMs: res.Elapsed.Milliseconds()})
	}
	return res
}

// run executes a command, capturing stderr for error messages.
// extraEnv, if non-empty, is appended to the current process environment.
func run(ctx context.Context, dir, extraEnv, name string, args ...string) error {
	var stderr bytes.Buffer
	cmd := exec.CommandContext(ctx, name, args...)
	cmd.Dir = dir
	cmd.Stderr = &stderr
	if extraEnv != "" {
		cmd.Env = append(os.Environ(), extraEnv)
	}
	if err := cmd.Run(); err != nil {
		msg := strings.TrimSpace(stderr.String())
		if msg != "" {
			return fmt.Errorf("%w\n%s", err, msg)
		}
		return err
	}
	return nil
}

// buildAll fans out builds across a worker pool and streams results as they complete.
func buildAll(ctx context.Context, repoURL string, branches []string, imageBase, dockerfile, gitSSHEnv string, push bool, numWorkers, numBuildWorkers int, hub *Hub) []BuildResult {
	// dockerSem caps concurrent docker build+prune operations.
	dockerSem := make(chan struct{}, numBuildWorkers)

	jobs := make(chan string, len(branches))
	for _, b := range branches {
		jobs <- b
	}
	close(jobs)

	resultsCh := make(chan BuildResult, len(branches))

	var wg sync.WaitGroup
	for i := 0; i < numWorkers; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for branch := range jobs {
				r := buildBranch(ctx, repoURL, branch, imageBase, dockerfile, gitSSHEnv, push, dockerSem, hub)
				if hub == nil {
					if r.Err != nil {
						fmt.Printf("[FAIL] %-50s (%v) %v\n", r.Branch, r.Elapsed.Round(time.Millisecond), r.Err)
					} else {
						fmt.Printf("[ OK ] %-50s -> %s (%v)\n", r.Branch, r.Image, r.Elapsed.Round(time.Millisecond))
					}
				}
				resultsCh <- r
			}
		}()
	}

	wg.Wait()
	close(resultsCh)

	results := make([]BuildResult, 0, len(branches))
	for r := range resultsCh {
		results = append(results, r)
	}
	return results
}
