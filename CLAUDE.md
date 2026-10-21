# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

**nestoa** is a static, deterministic OpenAPI generator for NestJS. A Rust binary (`crates/nestoa`)
parses TypeScript with swc, never compiling or running it, and writes `openapi.json`/`.yaml`. It is
distributed on npm through a JS shim (`npm/nestoa`) and prebuilt platform packages.

## Documentation

`docs/` is the reference. Read the relevant page before changing a behaviour, and update it in the
same PR as the change.

| Need | Page |
| --- | --- |
| Pipeline, modules, internal design | [`docs/architecture.md`](docs/architecture.md) |
| Which file to edit for a given change | [`docs/development.md`](docs/development.md#where-to-make-a-change) |
| What a config option does | [`docs/options/`](docs/options/README.md), one page per option |
| CLI flags, `--strict`, warnings | [`docs/cli.md`](docs/cli.md) |
| JSDoc tags | [`docs/jsdoc.md`](docs/jsdoc.md) |
| What the document looks like, 3.0.3 downgrade | [`docs/output.md`](docs/output.md) |

## Commands

Run `cargo` from the repository root.

```sh
make ci                                # mirrors the CI jobs: fmt check, clippy -D warnings, tests, release build
cargo test --package nestoa routes     # tests of one module
cargo test test_name                   # one test
cargo build --release
make bench BASELINE=<base binary>       # benchmark check, see docs/development.md#benchmark
./target/release/nestoa <project> -o out.yaml --strict
./target/release/nestoa <project> --print-config
```

## Validating a change

A change is done when all of the following hold. Report each result, including failures, in the PR.

1. **`make ci` passes.** Run it before every push; it mirrors the checks the CI workflow runs.
2. **Every behaviour change has a regression test.** Tests are inline `#[cfg(test)] mod tests`
   blocks in the module that changed, built on a small in-memory or temporary TypeScript project.
   Assert on the emitted schema, route or warning (`diag::take_warnings`), not just "no panic".
   Reproduce a bug in a test first, and watch it fail before fixing it.
3. **The binary is checked end to end.** Build the release binary and run it on a fixture project
   that exercises the change. Keep fixtures in the scratchpad, never in the repository. Check:
   - the exact output (paths, parameters, schemas, status codes), not only that it ran;
   - `--strict`: no new warning or `x-unresolved` unless the change is meant to add one;
   - determinism: two runs give byte-identical output.
4. **The output is valid OpenAPI**, for 3.1.0 and for `--openapi-version 3.0.3` when the change
   touches schemas, e.g. with `npx @redocly/cli lint` or `openapi-spec-validator`. A construct that
   is valid in only one of the two versions belongs in `dialect.rs`
   (see [output conventions](docs/output.md#openapi-31-and-30)).
5. **Real projects are diffed before and after.** Generate the documents of the real NestJS
   projects available to you with the base branch's binary and with yours, and explain every
   difference: intended by the change, or a regression. Compare `--strict` warnings and
   `x-unresolved` counts too. Summarise the effect in the PR with counts only, never the projects'
   names or code (see Confidentiality).
6. **The benchmark does not regress.** Run `make bench BASELINE=<base branch binary>` against the
   [NestJS OpenAPI generation benchmark](https://github.com/algoan/nestjs-openapi-generation-benchmark)
   (see [development](docs/development.md#benchmark)); the `Benchmark` CI job runs the same check.
   Report the core and total differences before and after. A category that gets more differences is a
   regression unless the change is meant to trade it, which the PR explains (`benchmark-regression`
   label). A change that closes a gap listed in the notes of the benchmark's `nestoa-main` variant
   updates those notes in the refresh pull request opened there after the merge.
7. **The docs match.** A new or changed option updates its page in `docs/options/` and the index;
   a new module or pipeline stage updates `docs/architecture.md`; a user-visible behaviour updates
   the matching guide.

## Confidentiality

nestoa is public. Code, tests, docs, commit messages, branch names, issues and PRs use generic
examples only: books, authors, pets, orders, widgets, `@acme/*` packages. Never copy or name the
services, packages, classes, fields or routes of a real project nestoa was run on. Describe a bug by
its TypeScript shape.

## Conventions

- Branches: `<type>/YY-MM-DD-<short-summary>`, with type `feat`, `fix`, `chore` or `docs`.
- Commits: Conventional Commits, title ≤ 60 characters, one logical change per commit, each passing
  `make ci`.
- PR titles: `[FEAT]`, `[FIX]`, `[CHORE]` or `[DOCS]` followed by the summary.
- Code comments explain *why* when it isn't obvious; the design rationale belongs in `docs/`.
