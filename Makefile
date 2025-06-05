CARGO := cargo

.PHONY: build
build:
	$(CARGO) build 

.PHONY: tests
tests: fmt-check clippy clippy-tests test doc-private

.PHONY: install-cargo-tools
install-cargo-tools:
	$(CARGO) install cargo-sort

.PHONY: fmt-check
fmt-check: install-cargo-tools
	$(CARGO) fmt -- --check
	$(CARGO) sort --check
	$(MAKE) -C integration_tests $@

.PHONY: clippy
clippy: install-cargo-tools
	$(CARGO) clippy -- \
		--deny clippy::all \
		--deny clippy::todo \
		--deny clippy::unimplemented \
		--deny clippy::print_stderr \
		--deny clippy::print_stdout

.PHONY: clippy-tests
clippy-tests: install-cargo-tools
	$(MAKE) -C integration_tests $@

.PHONY: fmt
fmt: install-cargo-tools
	$(CARGO) +nightly $@
	$(CARGO) sort
	$(MAKE) -C integration_tests $@

.PHONY: test
test: install-cargo-tools
	$(CARGO) test --all --exclude integration_tests

.PHONY: integration-tests
integration-tests:
	if [ -d "${HOME}/.local/share/cloudflare-warp-certs" ]; then \
  		export CF_CERTS="${HOME}/.local/share/cloudflare-warp-certs"; \
    fi; docker compose -f integration_tests/docker-compose.yaml up --build --force-recreate;

.PHONY: doc
doc:
	$(CARGO) doc --all-features

.PHONY: doc-private
doc-private:
	$(CARGO) doc --document-private-items --all-features

# build and open docs for local development
.PHONY: doc-open
doc-open:
	$(CARGO) doc --open --all-features

# build and open private docs for local development
.PHONY: doc-private-open
doc-private-open:
	$(CARGO) doc --document-private-items --open --all-features

.PHONY: audit
audit:
	$(CARGO) audit

.PHONY: install-release-reqs
install-release-reqs:
	$(CARGO) install cargo-release git-cliff

