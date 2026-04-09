package main

import (
	"context"
	"encoding/json"
	"os"
	"os/exec"
	"strings"
)

// BuildState persists the last-built commit SHA per branch across runs so that
// only branches with new commits are rebuilt.
type BuildState struct {
	BuiltSHAs map[string]string `json:"builtShas"`
}

func loadState(path string) BuildState {
	data, err := os.ReadFile(path)
	if err != nil {
		return BuildState{BuiltSHAs: make(map[string]string)}
	}
	var s BuildState
	if err := json.Unmarshal(data, &s); err != nil || s.BuiltSHAs == nil {
		return BuildState{BuiltSHAs: make(map[string]string)}
	}
	return s
}

// branchesToBuild returns branches that need a new image built.
// A branch is included if its SHA has changed since the last build OR if its
// image no longer exists — whichever comes first.
func branchesToBuild(ctx context.Context, branches []string, shas map[string]string, state BuildState, imageBase string, reg *Registry) []string {
	var build []string
	for _, b := range branches {
		if state.BuiltSHAs[b] != shas[b] || !imageExists(ctx, imageBase+":"+branchToTag(b), reg) {
			build = append(build, b)
		}
	}
	return build
}

// imageExists reports whether the given image reference is available.
// For the built-in registry it checks the tag file on disk; otherwise it
// asks Docker via `docker image inspect`.
func imageExists(ctx context.Context, image string, reg *Registry) bool {
	if reg != nil {
		// image = "localhost:PORT/name:tag" — split name and tag.
		i := strings.LastIndex(image, ":")
		if i < 0 {
			return false
		}
		tag := image[i+1:]
		name := image[:i]
		// Strip registry host prefix up to the first "/" after the host.
		if j := strings.Index(name, "/"); j >= 0 {
			name = name[j+1:]
		}
		return reg.HasTag(name, tag)
	}
	cmd := exec.CommandContext(ctx, "docker", "image", "inspect", "--format", "{{.Id}}", image)
	return cmd.Run() == nil
}

// saveState writes the state atomically via a temp-file rename.
func saveState(path string, s BuildState) error {
	data, err := json.MarshalIndent(s, "", "  ")
	if err != nil {
		return err
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, data, 0o644); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}
