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

# To create a release, use release-<version type> (patch, minor, or major; e.g make release-patch for a
# patch release). By default this only performs a dry run; to actually create a release specify EXECUTE 
# = true: make EXECUTE=true release. Once the release has been created, it can be pushed to crates.io 
# with push-release: make push-release (also a dry run by default).
#
# NOTE for Cloudflare employees: Warp causes issues when creating a release for crates.io, so make sure
# to disable it temporarily when doing this.
.PHONY: release-% push-release
release-%: EXECUTE ?= false
release-%: install-release-reqs
	$(CARGO) release $(if $(filter true,$(EXECUTE)),-x) --no-push $*

push-release: EXECUTE ?= false
push-release:
	$(CARGO) release push $(if $(filter true,$(EXECUTE)),-x)
