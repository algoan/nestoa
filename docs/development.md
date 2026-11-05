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

Unit tests are only part of validating a change: see the
[validation checklist in `CLAUDE.md`](../CLAUDE.md#validating-a-change) for the
end-to-end, `--strict`, determinism, OpenAPI and [benchmark](#benchmark) checks.

```sh
cargo test                             # everything
cargo test --package nestoa jsdoc      # one module
cargo test test_name                   # one test
```

The npm packaging scripts are tested with Node's test runner, in `npm/test/`:

```sh
make npm-test                          # node --test 'npm/test/*.test.js'
```

## Makefile

The `Makefile` mirrors `.github/workflows/ci.yml`, so CI can be reproduced
locally. `make help` lists every target.

```sh
make ci          # what CI runs: fmt check + clippy -D warnings + tests + npm tests + release build + npm smoke test
make test        # cargo test --workspace --locked
make build       # cargo build --release --workspace --locked
make lint        # fmt check + clippy
make fmt-fix     # apply formatting
make bench       # check against the benchmark, see below
```

## Benchmark

The [NestJS OpenAPI generation benchmark](https://github.com/algoan/nestjs-openapi-generation-benchmark)
compares nestoa with the other generators on a realistic NestJS app whose expected document is checked
against the running app. Its `nestoa-main` variant is the app configured for nestoa's main branch.

`scripts/benchmark.sh` generates that variant's document with a candidate binary and, when given, a
baseline one. It fails when the candidate has more differences with the expected document than the
baseline in any category (operations, parameters, request bodies, responses, errors, constraints,
security, docs), is not deterministic, or fails `--strict` where the baseline passes. The report lists
the new and fixed differences.

```sh
git clone https://github.com/algoan/nestjs-openapi-generation-benchmark.git ../nestjs-openapi-generation-benchmark
npm --prefix ../nestjs-openapi-generation-benchmark/bench ci
make bench                                        # differences of this branch
make bench BASELINE=path/to/main/nestoa           # fail on a regression against main's binary
make bench BENCHMARK_DIR=path/to/the/benchmark    # another checkout
```

The `Benchmark` workflow (`.github/workflows/benchmark.yml`) runs it on every pull request, against
a build of the base commit. To accept more differences or a `--strict` failure on purpose, explain it
in the pull request and add the `benchmark-regression` label; the script exits 1 for those, and 2 for
what is never accepted (non-determinism, no document written, a benchmark that cannot run).

The benchmark refreshes its results by itself: its refresh workflow polls nestoa every hour and, when
main has a new commit or `nestoa` a new version on npmjs.com, opens a pull request there with the results
of `nestoa-main` or of the released version. nestoa needs no secret for it, since both repositories and
the npm packages are public.

## Where to make a change

The code is organised so that most changes touch one file. See
[Architecture](architecture.md) for what each module does.

| Change | File |
| --- | --- |
| A config option | `config.rs`, plus its page in [`options/`](options/README.md) |
| A utility type or TypeScript type form | `schema.rs` |
| A class-property decorator (class-validator, class-transformer, `@nestjs/swagger`) | `schema.rs` |
| A JSDoc tag | `jsdoc.rs` |
| An HTTP decorator, parameter kind or pipe | `routes.rs` |
| An expression form in paths or decorator arguments | `constval.rs` |
| An interceptor transform or `unify` case | `interceptors.rs` |
| An error-response source or status mapping | `errors.rs` (signals gathered in `routes.rs`) |
| A module-resolution scheme (alias, package, namespace) | `symbols.rs`, plus `tsconfig.rs` for path aliases |
| Global prefix or versioning | `globalprefix.rs` |
| A 3.0.3 downgrade rule, or a draft-07 rewrite of AsyncAPI payloads | `dialect.rs` |
| A message-event decorator, publisher match or payload inference | `events.rs` |
| The AsyncAPI document's layout | `asyncapi.rs` |

## Release

Run the **Release** workflow (`.github/workflows/release.yml`) from the Actions
tab with the version to release, e.g. `0.3.0`; pushing a `v0.3.0` tag does the
same. It:

1. builds the binary for each target in its matrix: `linux-{x64,arm64}-{gnu,musl}`,
   `darwin-{x64,arm64}` and `win32-x64-msvc`. The glibc builds run on the oldest
   supported Ubuntu, so that they run on older glibc;
2. creates each `@algoan/nestoa-<tag>` platform package with
   `npm/build-platform-packages.js`, and smoke tests it with
   `npm/smoke-test.js`: packs it with the launcher, installs both with and
   without install scripts (in Alpine for musl) and runs the CLI;
3. publishes the packages to npmjs.com and to GitHub Packages.
   `npm/prepare-publish.js` stamps the version, names the launcher (`nestoa` on
   npmjs.com, `@algoan/nestoa` on GitHub Packages, which only accepts the
   owner's scope) and keeps only the built platform packages in its
   `optionalDependencies`. A prerelease version is published under the `next`
   dist-tag;
4. tags the commit (on a manual run), creates the GitHub release with the
   notes generated from the pull requests merged since the previous tag and
   the binaries attached. The benchmark picks up the new version by itself
   (see [Benchmark](#benchmark)).

Each publishing step skips what is already done, so a failed run can be
re-run. Artifacts are uploaded as tarballs, since `actions/upload-artifact`
drops the binaries' exec bit.

npmjs.com authenticates the workflow by trusted publishing (OIDC), configured
in each package's settings, and the packages carry a provenance attestation.
npm only allows configuring trusted publishing once a package exists, so the
workflow falls back to the `NPM_TOKEN` secret until it is.

A user only downloads the platform package that matches their machine. On a
platform without a platform package, there is no bundled binary to run. Set
`NESTOA_BINARY` to point the shim at a separately built executable; set it
during `npm install` too, since the shim is otherwise replaced by the binary
(see [npm distribution](architecture.md#npm-distribution)).

`make smoke` (part of `make ci`) runs the smoke test on the current machine,
and the CI's `npm install` job runs it on Linux, macOS and Windows.
