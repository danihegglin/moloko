package main

import (
	"encoding/json"
	"os"
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
