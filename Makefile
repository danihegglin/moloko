.PHONY: build ui clean server dashboard test-server

build: ui
	go build -o moloko .

ui:
	cd ui && npm install && npm run build

clean:
	rm -rf ui/dist ui/node_modules moloko

# Rust build server (embeds the dashboard into a single binary)
server: dashboard
	cd server && cargo build --release
	@echo "binary: server/target/release/moloko"

dashboard:
	cd dashboard && npm install && npm run build

test-server:
	cd server && cargo test
