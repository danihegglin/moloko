package main

import (
	"bytes"
	"context"
	"fmt"
	"io"
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

// RemoteBranch is a branch name paired with its current tip commit SHA.
type RemoteBranch struct {
	Name string
	SHA  string
}

// listBranches queries the remote repository without cloning it.
func listBranches(ctx context.Context, repoURL, gitSSHEnv string) ([]RemoteBranch, error) {
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
	var branches []RemoteBranch
	for _, line := range strings.Split(strings.TrimSpace(string(out)), "\n") {
		if line == "" {
			continue
		}
		// Each line: <sha>\trefs/heads/<branch>
		parts := strings.SplitN(line, "\t", 2)
		if len(parts) != 2 {
			continue
		}
		branches = append(branches, RemoteBranch{
			Name: strings.TrimPrefix(parts[1], "refs/heads/"),
			SHA:  parts[0],
		})
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
// cacheSources lists images to pass as --cache-from to docker build; the branch's
// own image is always prepended so re-runs of the same branch reuse prior layers.
func buildBranch(ctx context.Context, repoURL, branch, imageBase, dockerfile, gitSSHEnv string, push bool, dockerSem chan struct{}, hub *Hub, cacheSources []string) BuildResult {
	start := time.Now()
	image := imageBase + ":" + branchToTag(branch)
	res := BuildResult{Branch: branch, Image: image}

	// logW streams stdout+stderr of subcommands to the hub line by line.
	// nil in CLI mode (hub == nil) so output goes nowhere (errors still captured).
	var logW io.Writer
	if hub != nil {
		logW = &lineWriter{fn: func(line string) { hub.Log(branch, line) }}
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
	if err := run(ctx, tmpDir, gitSSHEnv, "git", logW,
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
	// Use select so a cancelled context unblocks immediately instead of hanging.
	select {
	case dockerSem <- struct{}{}:
	case <-ctx.Done():
		res.Err = ctx.Err()
		res.Elapsed = time.Since(start)
		if hub != nil {
			hub.Update(BranchState{Branch: branch, Phase: PhaseFailed, Image: image, ElapsedMs: res.Elapsed.Milliseconds(), Error: res.Err.Error()})
		}
		return res
	}

	// Build the argument list. Always try the branch's own previous image first
	// so re-runs skip unchanged layers. Then fall through to any supplied sources
	// (typically the main/master images built in the first phase of this run).
	buildArgs := []string{"build", "-t", image, "--cache-from", image}
	for _, src := range cacheSources {
		buildArgs = append(buildArgs, "--cache-from", src)
	}
	buildArgs = append(buildArgs, "-f", filepath.Join(cloneDir, dockerfile), cloneDir)

	buildErr := run(ctx, cloneDir, "", "docker", logW, buildArgs...)
	// Prune dangling build cache before releasing the slot so the next build
	// starts with freed space. Active layers are pinned and won't be removed.
	_ = run(ctx, "", "", "docker", nil, "builder", "prune", "-f")
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
		if err := run(ctx, cloneDir, "", "docker", logW, "push", image); err != nil {
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

// lineWriter calls fn for each complete line written to it.
// Safe for concurrent use — cmd.Stdout and cmd.Stderr may write simultaneously.
type lineWriter struct {
	mu  sync.Mutex
	buf []byte
	fn  func(string)
}

func (lw *lineWriter) Write(p []byte) (int, error) {
	lw.mu.Lock()
	defer lw.mu.Unlock()
	lw.buf = append(lw.buf, p...)
	for {
		i := bytes.IndexByte(lw.buf, '\n')
		if i < 0 {
			break
		}
		line := strings.TrimRight(string(lw.buf[:i]), "\r")
		lw.buf = lw.buf[i+1:]
		if line != "" {
			lw.fn(line)
		}
	}
	return len(p), nil
}

// run executes a command, capturing stderr for error reporting.
// If out is non-nil, both stdout and stderr are also streamed to it line by line.
// extraEnv, if non-empty, is appended to the current process environment.
func run(ctx context.Context, dir, extraEnv, name string, out io.Writer, args ...string) error {
	var errBuf bytes.Buffer
	cmd := exec.CommandContext(ctx, name, args...)
	cmd.Dir = dir
	if extraEnv != "" {
		cmd.Env = append(os.Environ(), extraEnv)
	}
	if out != nil {
		cmd.Stdout = out
		cmd.Stderr = io.MultiWriter(&errBuf, out)
	} else {
		cmd.Stderr = &errBuf
	}
	if err := cmd.Run(); err != nil {
		msg := strings.TrimSpace(errBuf.String())
		if msg != "" {
			return fmt.Errorf("%w\n%s", err, msg)
		}
		return err
	}
	return nil
}

// printResult logs a build result to stdout in CLI mode.
func printResult(r BuildResult) {
	if r.Err != nil {
		fmt.Printf("[FAIL] %-50s (%v) %v\n", r.Branch, r.Elapsed.Round(time.Millisecond), r.Err)
	} else {
		fmt.Printf("[ OK ] %-50s -> %s (%v)\n", r.Branch, r.Image, r.Elapsed.Round(time.Millisecond))
	}
}

// buildAll builds all branches in two phases:
//
//  1. Primary branches (main, master) are built sequentially first. Their
//     images seed the Docker layer cache for the branches that follow.
//
//  2. Feature branches run in parallel, each receiving the primary images as
//     --cache-from sources so dependency layers are reused without re-downloading.
func buildAll(ctx context.Context, repoURL string, branches []string, imageBase, dockerfile, gitSSHEnv string, push bool, numWorkers, numBuildWorkers int, hub *Hub) []BuildResult {
	dockerSem := make(chan struct{}, numBuildWorkers)

	var primary, feature []string
	for _, b := range branches {
		if b == "main" || b == "master" {
			primary = append(primary, b)
		} else {
			feature = append(feature, b)
		}
	}

	var results []BuildResult

	// Phase 1 — primary branches, sequential, no external cache sources yet.
	var cacheImages []string
	for _, b := range primary {
		r := buildBranch(ctx, repoURL, b, imageBase, dockerfile, gitSSHEnv, push, dockerSem, hub, nil)
		if hub == nil {
			printResult(r)
		}
		results = append(results, r)
		if r.Err == nil {
			cacheImages = append(cacheImages, r.Image)
		}
	}

	if len(feature) == 0 {
		return results
	}

	// Phase 2 — feature branches, parallel, seeded from primary image cache.
	jobs := make(chan string, len(feature))
	for _, b := range feature {
		jobs <- b
	}
	close(jobs)

	resultsCh := make(chan BuildResult, len(feature))
	var wg sync.WaitGroup
	for i := 0; i < numWorkers; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for branch := range jobs {
				r := buildBranch(ctx, repoURL, branch, imageBase, dockerfile, gitSSHEnv, push, dockerSem, hub, cacheImages)
				if hub == nil {
					printResult(r)
				}
				resultsCh <- r
			}
		}()
	}

	wg.Wait()
	close(resultsCh)

	for r := range resultsCh {
		results = append(results, r)
	}
	return results
}
