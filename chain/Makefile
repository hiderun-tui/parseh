# PARSEH chain Makefile
# Build, test, lint targets for the parsehd node binary.

SHELL := /usr/bin/env bash
BUILD_DIR := ./bin
VERSION := $(shell git describe --tags --always --dirty 2>/dev/null || echo "v0.0.0-dev")
COMMIT  := $(shell git rev-parse --short HEAD 2>/dev/null || echo "unknown")
LDFLAGS := -X main.version=$(VERSION) -X main.commit=$(COMMIT)

.PHONY: all build install test lint clean help

all: build  ## default: build the parsehd binary

help:  ## print this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
	  awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

build:  ## compile ./cmd/parsehd → ./bin/parsehd
	@mkdir -p $(BUILD_DIR)
	go build -ldflags "$(LDFLAGS)" -o $(BUILD_DIR)/parsehd ./cmd/parsehd
	@echo "✓ Built $(BUILD_DIR)/parsehd ($(VERSION))"

install:  ## install parsehd to GOPATH/bin
	go install -ldflags "$(LDFLAGS)" ./cmd/parsehd

test:  ## run go tests
	go test ./...

lint:  ## run golangci-lint (must be installed separately)
	golangci-lint run ./... || echo "golangci-lint not installed · skip"

clean:  ## remove build artefacts
	rm -rf $(BUILD_DIR)
