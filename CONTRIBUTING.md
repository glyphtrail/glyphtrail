# Contributing to glyphtrail

## Branching: GitHub Flow

This repo uses [GitHub Flow](https://docs.github.com/en/get-started/using-github/github-flow):

- `main` is always releasable. Never commit to it directly.
- Branch off the latest `main` with a `feature/` prefix and a short kebab-case
  name, e.g. `feature/pin-lbug-prebuilt-version`. Use `feature/` for every
  change, including fixes, docs and dependency bumps. Dependabot's own
  `dependabot/...` branches are the only exception.
- Open a pull request against `main`. CI (`fmt-clippy-test` on stable and the
  MSRV toolchain) must be green before merging.
- PRs are squash-merged, so the PR title becomes the commit subject on `main`.
  Write it as a plain imperative sentence. The branch is deleted on merge.
- Releases are cut from `main`. There are no long-lived release or develop
  branches.

## Vocabulary

[`GLOSSARY.md`](GLOSSARY.md) defines the project's domain terms (Index, Seed,
Blast Radius, Endpoint, Cross-Repo Link, ...) and lists synonyms to avoid. Use
those terms in code, docs, issues and PR titles, and extend the glossary when a
new concept settles.

## Before opening a PR

```sh
task fmt   # rustfmt + sync bundled agent assets
task ci    # fmt check, clippy -D warnings, cargo check, tests
```

If you edit `.claude/skills/glyphtrail/SKILL.md` or the managed section in
`CLAUDE.md`, run `task assets:sync` (or `task fmt`) so the copies bundled under
`crates/glyphtrail-cli/assets/` stay in sync.
