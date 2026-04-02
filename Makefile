.PHONY: build ui clean

build: ui
	go build -o moloko .

ui:
	cd ui && npm install && npm run build

clean:
	rm -rf ui/dist ui/node_modules moloko
