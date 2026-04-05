package main

import (
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"hash"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
)

// Registry implements the OCI Distribution Spec v2 for local image storage.
// Blobs are stored content-addressably; uploads stream directly to disk with
// a running SHA256 so nothing is buffered in memory.
type Registry struct {
	dir     string
	mu      sync.RWMutex
	uploads map[string]*activeUpload
}

type activeUpload struct {
	mu     sync.Mutex
	tmp    string   // temp file path
	file   *os.File
	offset int64
	sha    hash.Hash
}

// NewRegistry creates the storage layout under dir and returns a Registry.
func NewRegistry(dir string) (*Registry, error) {
	for _, sub := range []string{
		filepath.Join("blobs", "sha256"),
		"manifests",
		"uploads",
	} {
		if err := os.MkdirAll(filepath.Join(dir, sub), 0o755); err != nil {
			return nil, fmt.Errorf("registry init: %w", err)
		}
	}
	return &Registry{dir: dir, uploads: make(map[string]*activeUpload)}, nil
}

func (r *Registry) ServeHTTP(w http.ResponseWriter, req *http.Request) {
	w.Header().Set("Docker-Distribution-API-Version", "registry/2.0")

	tail := strings.TrimPrefix(req.URL.Path, "/v2")
	if tail == "" || tail == "/" {
		if req.Method == http.MethodGet || req.Method == http.MethodHead {
			w.WriteHeader(http.StatusOK)
			return
		}
		w.WriteHeader(http.StatusMethodNotAllowed)
		return
	}

	name, resource, rest := splitRegistryPath(strings.TrimPrefix(tail, "/"))
	if resource == "" {
		regErr(w, http.StatusNotFound, "NOT_FOUND", "path not found")
		return
	}

	switch resource {
	case "blobs":
		r.serveBlobs(w, req, name, rest)
	case "manifests":
		r.serveManifests(w, req, name, strings.Join(rest, "/"))
	case "tags":
		r.serveTags(w, req, name)
	default:
		regErr(w, http.StatusNotFound, "NOT_FOUND", "unknown resource type")
	}
}

// ── Blobs ────────────────────────────────────────────────────────────────────

func (r *Registry) serveBlobs(w http.ResponseWriter, req *http.Request, name string, rest []string) {
	if len(rest) == 0 {
		regErr(w, http.StatusBadRequest, "UNSUPPORTED", "missing blob reference")
		return
	}
	if rest[0] == "uploads" {
		r.serveUploads(w, req, name, rest[1:])
		return
	}
	digest := rest[0]
	path := r.blobPath(digest)

	switch req.Method {
	case http.MethodHead:
		info, err := os.Stat(path)
		if err != nil {
			regErr(w, http.StatusNotFound, "BLOB_UNKNOWN", "blob unknown")
			return
		}
		w.Header().Set("Content-Length", fmt.Sprint(info.Size()))
		w.Header().Set("Docker-Content-Digest", digest)
		w.WriteHeader(http.StatusOK)

	case http.MethodGet:
		f, err := os.Open(path)
		if err != nil {
			regErr(w, http.StatusNotFound, "BLOB_UNKNOWN", "blob unknown")
			return
		}
		defer f.Close()
		info, _ := f.Stat()
		w.Header().Set("Docker-Content-Digest", digest)
		w.Header().Set("Content-Type", "application/octet-stream")
		http.ServeContent(w, req, "", info.ModTime(), f)

	case http.MethodDelete:
		if err := os.Remove(path); err != nil {
			regErr(w, http.StatusNotFound, "BLOB_UNKNOWN", "blob unknown")
			return
		}
		w.WriteHeader(http.StatusAccepted)

	default:
		w.WriteHeader(http.StatusMethodNotAllowed)
	}
}

// ── Uploads ──────────────────────────────────────────────────────────────────

func (r *Registry) serveUploads(w http.ResponseWriter, req *http.Request, name string, rest []string) {
	uuid := ""
	if len(rest) > 0 {
		uuid = rest[0]
	}

	switch req.Method {
	case http.MethodPost:
		// Cross-repository blob mount: if the blob is already present, skip upload.
		if mount := req.URL.Query().Get("mount"); mount != "" {
			if r.tryMount(w, req, name, mount) {
				return
			}
		}
		// Monolithic upload: POST with body + ?digest=sha256:... in one shot.
		if digest := req.URL.Query().Get("digest"); digest != "" {
			r.monolithicUpload(w, req, name, digest)
			return
		}
		r.startUpload(w, req, name)

	case http.MethodPatch:
		if uuid == "" {
			regErr(w, http.StatusBadRequest, "UNSUPPORTED", "missing upload uuid")
			return
		}
		r.patchUpload(w, req, name, uuid)

	case http.MethodPut:
		if uuid == "" {
			regErr(w, http.StatusBadRequest, "UNSUPPORTED", "missing upload uuid")
			return
		}
		r.finishUpload(w, req, name, uuid)

	case http.MethodDelete:
		r.cancelUpload(w, req, uuid)

	default:
		w.WriteHeader(http.StatusMethodNotAllowed)
	}
}

func (r *Registry) tryMount(w http.ResponseWriter, _ *http.Request, name, digest string) bool {
	if _, err := os.Stat(r.blobPath(digest)); err != nil {
		return false
	}
	w.Header().Set("Docker-Content-Digest", digest)
	w.Header().Set("Location", fmt.Sprintf("/v2/%s/blobs/%s", name, digest))
	w.WriteHeader(http.StatusCreated)
	return true
}

func (r *Registry) monolithicUpload(w http.ResponseWriter, req *http.Request, name, expectedDigest string) {
	h := sha256.New()
	tmp := filepath.Join(r.dir, "uploads", genUUID())
	f, err := os.Create(tmp)
	if err != nil {
		regErr(w, http.StatusInternalServerError, "UNKNOWN", "failed to create temp file")
		return
	}
	if _, err := io.Copy(io.MultiWriter(f, h), req.Body); err != nil {
		f.Close()
		os.Remove(tmp)
		regErr(w, http.StatusInternalServerError, "UNKNOWN", "upload write failed")
		return
	}
	f.Close()

	digest := "sha256:" + hex.EncodeToString(h.Sum(nil))
	if expectedDigest != digest {
		os.Remove(tmp)
		regErr(w, http.StatusBadRequest, "DIGEST_INVALID", "digest mismatch")
		return
	}
	dst := r.blobPath(digest)
	if err := os.MkdirAll(filepath.Dir(dst), 0o755); err != nil || os.Rename(tmp, dst) != nil {
		os.Remove(tmp)
		regErr(w, http.StatusInternalServerError, "UNKNOWN", "blob store error")
		return
	}
	w.Header().Set("Docker-Content-Digest", digest)
	w.Header().Set("Location", fmt.Sprintf("/v2/%s/blobs/%s", name, digest))
	w.WriteHeader(http.StatusCreated)
}

func (r *Registry) startUpload(w http.ResponseWriter, _ *http.Request, name string) {
	id := genUUID()
	tmp := filepath.Join(r.dir, "uploads", id)
	f, err := os.Create(tmp)
	if err != nil {
		regErr(w, http.StatusInternalServerError, "UNKNOWN", "failed to create upload")
		return
	}
	r.mu.Lock()
	r.uploads[id] = &activeUpload{tmp: tmp, file: f, sha: sha256.New()}
	r.mu.Unlock()

	w.Header().Set("Location", fmt.Sprintf("/v2/%s/blobs/uploads/%s", name, id))
	w.Header().Set("Docker-Upload-UUID", id)
	w.Header().Set("Range", "0-0")
	w.WriteHeader(http.StatusAccepted)
}

func (r *Registry) patchUpload(w http.ResponseWriter, req *http.Request, name, id string) {
	r.mu.RLock()
	u, ok := r.uploads[id]
	r.mu.RUnlock()
	if !ok {
		regErr(w, http.StatusNotFound, "BLOB_UPLOAD_UNKNOWN", "upload not found")
		return
	}

	u.mu.Lock()
	defer u.mu.Unlock()

	n, err := io.Copy(io.MultiWriter(u.file, u.sha), req.Body)
	if err != nil {
		regErr(w, http.StatusInternalServerError, "UNKNOWN", "upload write failed")
		return
	}
	u.offset += n

	w.Header().Set("Location", fmt.Sprintf("/v2/%s/blobs/uploads/%s", name, id))
	w.Header().Set("Docker-Upload-UUID", id)
	w.Header().Set("Range", fmt.Sprintf("0-%d", u.offset-1))
	w.WriteHeader(http.StatusAccepted)
}

func (r *Registry) finishUpload(w http.ResponseWriter, req *http.Request, name, id string) {
	r.mu.RLock()
	u, ok := r.uploads[id]
	r.mu.RUnlock()
	if !ok {
		regErr(w, http.StatusNotFound, "BLOB_UPLOAD_UNKNOWN", "upload not found")
		return
	}

	u.mu.Lock()
	defer func() {
		u.mu.Unlock()
		r.mu.Lock()
		delete(r.uploads, id)
		r.mu.Unlock()
	}()

	// PUT may carry a final chunk.
	if req.Body != nil && req.Body != http.NoBody {
		io.Copy(io.MultiWriter(u.file, u.sha), req.Body) //nolint
	}
	u.file.Close()

	digest := "sha256:" + hex.EncodeToString(u.sha.Sum(nil))
	if expected := req.URL.Query().Get("digest"); expected != "" && expected != digest {
		os.Remove(u.tmp)
		regErr(w, http.StatusBadRequest, "DIGEST_INVALID",
			fmt.Sprintf("expected %s got %s", expected, digest))
		return
	}

	dst := r.blobPath(digest)
	if err := os.MkdirAll(filepath.Dir(dst), 0o755); err != nil {
		os.Remove(u.tmp)
		regErr(w, http.StatusInternalServerError, "UNKNOWN", "blob store error")
		return
	}
	if err := os.Rename(u.tmp, dst); err != nil {
		os.Remove(u.tmp)
		regErr(w, http.StatusInternalServerError, "UNKNOWN", "blob rename error")
		return
	}

	w.Header().Set("Docker-Content-Digest", digest)
	w.Header().Set("Location", fmt.Sprintf("/v2/%s/blobs/%s", name, digest))
	w.WriteHeader(http.StatusCreated)
}

func (r *Registry) cancelUpload(w http.ResponseWriter, _ *http.Request, id string) {
	r.mu.Lock()
	u, ok := r.uploads[id]
	if ok {
		delete(r.uploads, id)
	}
	r.mu.Unlock()
	if ok {
		u.file.Close()
		os.Remove(u.tmp)
	}
	w.WriteHeader(http.StatusNoContent)
}

// ── Manifests ────────────────────────────────────────────────────────────────

func (r *Registry) serveManifests(w http.ResponseWriter, req *http.Request, name, ref string) {
	switch req.Method {
	case http.MethodHead, http.MethodGet:
		r.getManifest(w, req, name, ref)
	case http.MethodPut:
		r.putManifest(w, req, name, ref)
	case http.MethodDelete:
		r.deleteManifest(w, req, name, ref)
	default:
		w.WriteHeader(http.StatusMethodNotAllowed)
	}
}

func (r *Registry) getManifest(w http.ResponseWriter, req *http.Request, name, ref string) {
	// Resolve tag → digest.
	if !strings.HasPrefix(ref, "sha256:") {
		raw, err := os.ReadFile(r.tagPath(name, ref))
		if err != nil {
			regErr(w, http.StatusNotFound, "MANIFEST_UNKNOWN", "manifest unknown")
			return
		}
		ref = strings.TrimSpace(string(raw))
	}

	data, err := os.ReadFile(r.manifestPath(name, ref))
	if err != nil {
		regErr(w, http.StatusNotFound, "MANIFEST_UNKNOWN", "manifest unknown")
		return
	}

	w.Header().Set("Content-Type", manifestContentType(data))
	w.Header().Set("Docker-Content-Digest", ref)
	w.Header().Set("Content-Length", fmt.Sprint(len(data)))
	if req.Method == http.MethodHead {
		w.WriteHeader(http.StatusOK)
		return
	}
	w.WriteHeader(http.StatusOK)
	w.Write(data) //nolint
}

func (r *Registry) putManifest(w http.ResponseWriter, req *http.Request, name, ref string) {
	data, err := io.ReadAll(io.LimitReader(req.Body, 10<<20))
	if err != nil {
		regErr(w, http.StatusBadRequest, "MANIFEST_INVALID", "failed to read manifest")
		return
	}

	sum := sha256.Sum256(data)
	digest := "sha256:" + hex.EncodeToString(sum[:])

	// Store manifest by digest.
	mpath := r.manifestPath(name, digest)
	if err := os.MkdirAll(filepath.Dir(mpath), 0o755); err != nil {
		regErr(w, http.StatusInternalServerError, "UNKNOWN", "storage error")
		return
	}
	if err := atomicWriteFile(mpath, data); err != nil {
		regErr(w, http.StatusInternalServerError, "UNKNOWN", "manifest write error")
		return
	}

	// For a tag reference, store the tag → digest pointer.
	if !strings.HasPrefix(ref, "sha256:") {
		tpath := r.tagPath(name, ref)
		if err := os.MkdirAll(filepath.Dir(tpath), 0o755); err != nil {
			regErr(w, http.StatusInternalServerError, "UNKNOWN", "storage error")
			return
		}
		if err := atomicWriteFile(tpath, []byte(digest)); err != nil {
			regErr(w, http.StatusInternalServerError, "UNKNOWN", "tag write error")
			return
		}
	}

	w.Header().Set("Docker-Content-Digest", digest)
	w.Header().Set("Location", fmt.Sprintf("/v2/%s/manifests/%s", name, digest))
	w.WriteHeader(http.StatusCreated)
}

func (r *Registry) deleteManifest(w http.ResponseWriter, _ *http.Request, name, ref string) {
	if !strings.HasPrefix(ref, "sha256:") {
		raw, err := os.ReadFile(r.tagPath(name, ref))
		if err != nil {
			regErr(w, http.StatusNotFound, "MANIFEST_UNKNOWN", "manifest unknown")
			return
		}
		os.Remove(r.tagPath(name, ref))
		ref = strings.TrimSpace(string(raw))
	}
	if err := os.Remove(r.manifestPath(name, ref)); err != nil {
		regErr(w, http.StatusNotFound, "MANIFEST_UNKNOWN", "manifest unknown")
		return
	}
	w.WriteHeader(http.StatusAccepted)
}

// ── Tags ─────────────────────────────────────────────────────────────────────

func (r *Registry) serveTags(w http.ResponseWriter, req *http.Request, name string) {
	if req.Method != http.MethodGet {
		w.WriteHeader(http.StatusMethodNotAllowed)
		return
	}
	tagsDir := filepath.Join(r.dir, "manifests", filepath.FromSlash(name), "tags")
	entries, _ := os.ReadDir(tagsDir)
	tags := make([]string, 0, len(entries))
	for _, e := range entries {
		if !e.IsDir() {
			tags = append(tags, e.Name())
		}
	}
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(map[string]any{"name": name, "tags": tags}) //nolint
}

// HasTag reports whether a named image tag exists in the registry.
func (r *Registry) HasTag(name, tag string) bool {
	_, err := os.Stat(r.tagPath(name, tag))
	return err == nil
}

// ── Storage paths ─────────────────────────────────────────────────────────────

func (r *Registry) blobPath(digest string) string {
	return filepath.Join(r.dir, "blobs", "sha256", strings.TrimPrefix(digest, "sha256:"))
}

func (r *Registry) manifestPath(name, digest string) string {
	return filepath.Join(r.dir, "manifests", filepath.FromSlash(name), "sha256",
		strings.TrimPrefix(digest, "sha256:"))
}

func (r *Registry) tagPath(name, tag string) string {
	return filepath.Join(r.dir, "manifests", filepath.FromSlash(name), "tags", tag)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

// splitRegistryPath splits a v2 API path into (name, resource, rest).
// e.g. "myimage/blobs/sha256:abc" → ("myimage", "blobs", ["sha256:abc"])
// e.g. "ns/img/manifests/latest"  → ("ns/img", "manifests", ["latest"])
func splitRegistryPath(path string) (name, resource string, rest []string) {
	parts := strings.Split(path, "/")
	for i, p := range parts {
		switch p {
		case "blobs", "manifests", "tags":
			return strings.Join(parts[:i], "/"), p, parts[i+1:]
		}
	}
	return "", "", nil
}

func manifestContentType(data []byte) string {
	var m struct {
		MediaType string `json:"mediaType"`
	}
	if json.Unmarshal(data, &m) == nil && m.MediaType != "" {
		return m.MediaType
	}
	return "application/vnd.docker.distribution.manifest.v2+json"
}

func atomicWriteFile(path string, data []byte) error {
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, data, 0o644); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

func genUUID() string {
	b := make([]byte, 16)
	if _, err := rand.Read(b); err != nil {
		panic(err)
	}
	return fmt.Sprintf("%08x-%04x-%04x-%04x-%012x",
		b[0:4], b[4:6], b[6:8], b[8:10], b[10:16])
}

func regErr(w http.ResponseWriter, status int, code, msg string) {
	type regError struct {
		Code    string `json:"code"`
		Message string `json:"message"`
	}
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	json.NewEncoder(w).Encode(map[string]any{ //nolint
		"errors": []regError{{Code: code, Message: msg}},
	})
}
