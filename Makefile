all:
	cargo build
	cd examples/seed && ../../target/debug/trunk -v serve