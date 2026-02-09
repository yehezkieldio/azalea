# List available just commands
default:
    @just --list

cargo := "cargo"

# Run all checks (clippy, fmt, and tests)
all: fmt clippy test

# Check code for compilation errors
check:
    {{cargo}} check --workspace

# Run clippy with strict warnings
clippy *args:
    {{cargo}} clippy --all-targets --all-features {{args}} -- -D warnings

# Run all tests using nextest
test *args:
    {{cargo}} nextest run {{args}}

# Run integration tests specifically
test-integ:
    {{cargo}} nextest run -E 'binary(integration_test)'

# Format code and check for style issues
fmt:
    {{cargo}} fmt --all

# Check formatting without applying changes
fmt-check:
    {{cargo}} fmt --all -- --check

# Run the binary with optional arguments
run *args:
    {{cargo}} run -p azalea {{args}}