package main

import (
	"context"
	"encoding/json"
	"fmt"
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

// reconcileState drops BuiltSHAs entries whose image no longer exists,
// ensuring stale state (after a prune, registry wipe, or machine move) doesn't
// skip rebuilds for branches that actually need one.
func reconcileState(ctx context.Context, branches []string, state BuildState, imageBase string, reg *Registry) BuildState {
	for _, b := range branches {
		if _, recorded := state.BuiltSHAs[b]; !recorded {
			continue
		}
		image := imageBase + ":" + branchToTag(b)
		if !imageExists(ctx, image, reg) {
			fmt.Printf("  stale: image %s not found — will rebuild %s\n", image, b)
			delete(state.BuiltSHAs, b)
		}
	}
	return state
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
