.PHONY: clippy-tests rustfmt fmt-check
clippy-tests:
	cargo clippy --tests -- \
		--deny warnings \
		--deny clippy::all \
		--deny clippy::todo \
		--deny clippy::unimplemented

fmt:
	cargo +nightly $@
	cargo sort

fmt-check:
	cargo fmt -- --check
	cargo sort --check
