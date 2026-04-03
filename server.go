package main

import (
	"context"
	"embed"
	"encoding/json"
	"fmt"
	"io/fs"
	"net/http"
	"sync"
)

//go:embed ui/dist
var uiDist embed.FS

// Phase represents the current state of a branch build.
type Phase string

const (
	PhaseQueued   Phase = "queued"
	PhaseCloning  Phase = "cloning"
	PhaseBuilding Phase = "building"
	PhasePushing  Phase = "pushing"
	PhaseDone     Phase = "done"
	PhaseFailed   Phase = "failed"
)

// BranchState holds the current build state for a single branch.
type BranchState struct {
	Branch    string `json:"branch"`
	Phase     Phase  `json:"phase"`
	Image     string `json:"image,omitempty"`
	ElapsedMs int64  `json:"elapsedMs,omitempty"`
	Error     string `json:"error,omitempty"`
}

// Summary holds overall build results sent when all branches are done.
type Summary struct {
	Succeeded int   `json:"succeeded"`
	Failed    int   `json:"failed"`
	ElapsedMs int64 `json:"elapsedMs"`
}

// LogLine is a single log line emitted from a branch build.
type LogLine struct {
	Branch string `json:"branch"`
	Line   string `json:"line"`
}

// Hub manages SSE clients and branch build state.
type Hub struct {
	repo     string
	branches []string
	state    map[string]BranchState
	logs     map[string][]string
	clients  map[chan string]struct{}
	summary  *Summary
	mu       sync.Mutex
}

// NewHub creates a Hub with all branches initialised as PhaseQueued.
func NewHub(repo string, branches []string) *Hub {
	h := &Hub{
		repo:     repo,
		branches: branches,
		state:    make(map[string]BranchState, len(branches)),
		logs:     make(map[string][]string, len(branches)),
		clients:  make(map[chan string]struct{}),
	}
	for _, b := range branches {
		h.state[b] = BranchState{Branch: b, Phase: PhaseQueued}
	}
	return h
}

// Log appends a log line for a branch and broadcasts it to all clients.
func (h *Hub) Log(branch, line string) {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.logs[branch] = append(h.logs[branch], line)
	h.broadcast("log", LogLine{Branch: branch, Line: line})
}

// Update stores a branch state and broadcasts an "update" event to all clients.
func (h *Hub) Update(s BranchState) {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.state[s.Branch] = s
	h.broadcast("update", s)
}

// Finish stores the summary and broadcasts a "done" event to all clients.
func (h *Hub) Finish(s Summary) {
	h.mu.Lock()
	defer h.mu.Unlock()
	h.summary = &s
	h.broadcast("done", s)
}

// broadcast sends an SSE message to all connected clients.
// Must be called with h.mu held.
func (h *Hub) broadcast(event string, data any) {
	msg := sseJSON(event, data)
	for ch := range h.clients {
		select {
		case ch <- msg:
		default:
		}
	}
}

type initPayload struct {
	Repo     string              `json:"repo"`
	Branches []string            `json:"branches"`
	States   []BranchState       `json:"states"`
	Logs     map[string][]string `json:"logs"`
}

// subscribe registers a new SSE client and returns its channel plus an unsubscribe func.
// It snapshots the current state and sends an "init" message before returning, so the
// client never misses any updates.
func (h *Hub) subscribe() (chan string, func()) {
	ch := make(chan string, 64)

	h.mu.Lock()

	states := make([]BranchState, 0, len(h.branches))
	for _, b := range h.branches {
		states = append(states, h.state[b])
	}
	logsCopy := make(map[string][]string, len(h.logs))
	for branch, lines := range h.logs {
		if len(lines) > 0 {
			cp := make([]string, len(lines))
			copy(cp, lines)
			logsCopy[branch] = cp
		}
	}
	ch <- sseJSON("init", initPayload{
		Repo:     h.repo,
		Branches: h.branches,
		States:   states,
		Logs:     logsCopy,
	})

	if h.summary != nil {
		ch <- sseJSON("done", *h.summary)
	}

	h.clients[ch] = struct{}{}

	h.mu.Unlock()

	unsub := func() {
		h.mu.Lock()
		delete(h.clients, ch)
		h.mu.Unlock()
	}
	return ch, unsub
}

// sseJSON formats a server-sent event message as a data line.
func sseJSON(event string, data any) string {
	b, _ := json.Marshal(map[string]any{"event": event, "data": data})
	return "data: " + string(b) + "\n\n"
}

// ServeSSE handles an SSE connection, streaming build events to the client.
func (h *Hub) ServeSSE(w http.ResponseWriter, r *http.Request) {
	flusher, ok := w.(http.Flusher)
	if !ok {
		http.Error(w, "streaming unsupported", http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	w.Header().Set("X-Accel-Buffering", "no")

	ch, unsub := h.subscribe()
	defer unsub()

	for {
		select {
		case msg, ok := <-ch:
			if !ok {
				return
			}
			fmt.Fprint(w, msg)
			flusher.Flush()
		case <-r.Context().Done():
			return
		}
	}
}

// startServer creates and starts the HTTP server serving the UI and SSE endpoint.
// It shuts down gracefully when ctx is cancelled.
func startServer(ctx context.Context, addr string, hub *Hub) error {
	mux := http.NewServeMux()

	mux.HandleFunc("/events", hub.ServeSSE)

	sub, err := fs.Sub(uiDist, "ui/dist")
	if err != nil {
		return fmt.Errorf("embed sub: %w", err)
	}
	mux.Handle("/", http.FileServer(http.FS(sub)))

	srv := &http.Server{
		Addr:    addr,
		Handler: mux,
	}

	errCh := make(chan error, 1)
	go func() {
		if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			errCh <- err
		}
		close(errCh)
	}()

	select {
	case <-ctx.Done():
		return srv.Shutdown(context.Background())
	case err := <-errCh:
		return err
	}
}
