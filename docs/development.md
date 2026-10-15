# Development

## Build and run

Run every `cargo` command from the repository root.

```sh
cargo build --release                  # → target/release/nestoa
./target/release/nestoa path/to/project
```

To run the npm shim against a local build without publishing:

```sh
NESTOA_BINARY=$PWD/target/release/nestoa node npm/nestoa/bin/nestoa.js . -o /tmp/out.json
```

## Tests

Tests are inline `#[cfg(test)] mod tests` blocks in each module, not a separate
`tests/` directory. Most build a small in-memory or temporary project and
assert on the emitted schema or route.

```sh
cargo test                             # everything
cargo test --package nestoa jsdoc      # one module
cargo test test_name                   # one test
```

## Makefile

The `Makefile` mirrors `.github/workflows/ci.yml`, so CI can be reproduced
locally. `make help` lists every target.

```sh
make ci          # what CI runs: fmt check + clippy -D warnings + tests + release build
make test        # cargo test --workspace --locked
make build       # cargo build --release --workspace --locked
make lint        # fmt check + clippy
make fmt-fix     # apply formatting
```

## Where to make a change

The code is organised so that most changes touch one file. See
[Architecture](architecture.md) for what each module does.

| Change | File |
| --- | --- |
| A config option | `config.rs`, plus its page in [`options/`](options/README.md) |
| A utility type or TypeScript type form | `schema.rs` |
| A JSDoc tag | `jsdoc.rs` |
| An HTTP decorator, parameter kind or pipe | `routes.rs` |
| An expression form in paths or decorator arguments | `constval.rs` |
| An interceptor transform or `unify` case | `interceptors.rs` |
| An error-response source or status mapping | `errors.rs` (signals gathered in `routes.rs`) |
| A module-resolution scheme (alias, package, namespace) | `symbols.rs`, plus `tsconfig.rs` for path aliases |
| Global prefix or versioning | `globalprefix.rs` |
| A 3.0.3 downgrade rule | `dialect.rs` |

## Release

Each release builds the binary for every target listed in
`npm/build-platform-packages.js`. It then runs that script once per target to
create the `@nestoa/<tag>` packages, and publishes `nestoa` and every
`@nestoa/<tag>` at the same version. `npm/prepare-github-publish.js` renames
them for the GitHub Packages registry (`@algoan/nestoa`,
`@algoan/nestoa-<tag>`). A user only ever downloads the platform package that
matches their machine.
