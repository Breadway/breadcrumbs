# CLAUDE.md — Repo hygiene

Scope: this file covers *repo hygiene* — branching, remotes, CI, cleanup. It is not project documentation.

This repo follows the branch/release workflow documented in `CONTRIBUTING.md`
— read and follow it for any git, branch, or release work here (the
single-trunk model, `feature/x`/`fix/x` branch naming, how RC tags work,
etc). Don't improvise a different workflow. The short version: there is one
long-lived branch, `main` — no `dev` or `beta` branch exists. `main`
auto-publishes a dev-track build on every push. "Beta" and "stable" are both
just tags, not branches: push a `vX.Y.Z-rc.N` tag to publish a beta-track
build, push a plain `vX.Y.Z` tag to cut the signed stable release.
"Freezing" for stabilization means pausing pushes to `main`, not moving a
branch. This replaced an earlier three-branch (`dev`/`beta`/`main`) model
after `main` was found to have silently rotted out of sync with `dev`/`beta`
across most repos in this ecosystem — a manual "merge beta into main
monthly" step nobody reliably did across a dozen-plus repos. Collapsing to
one branch removes the class of bug; there's nothing left that can fall out
of sync.

## Remotes
- `origin` — Forgejo (`git.breadway.dev` via Hestia, SSH) — authoritative.
- `github` — GitHub mirror. Push both when publishing. Agents push `origin` only; the GitHub remote auto-mirrors.

## Distribution
- Bakery-only. `bakery.toml` is the product manifest; there is no
  `packaging/arch/PKGBUILD` in this repo.
- Tracks: `bakery track set {dev,beta,stable}` then `bakery update breadcrumbs`
  (or `bakery update --all`). See CONTRIBUTING.md and bread-ecosystem's
  `docs/release-channels.md`.

## Events
- Bread bus contract: `EVENTS.md`. App id is `crumbs`.
- Fail-silent: breadcrumbs behaves the same whether `breadd` is running or
  not. Commands (`bread.command.crumbs.*`) are only received while
  `breadcrumbs watch` / the user systemd unit is up.

## CI
- `check.yml` — clippy + `cargo test --release` on `feature/**` and `fix/**`.
- `dev-release.yml` — triggered on push to `main` (dev-track bakery publish).
- `rc-release.yml` — triggered on any `vX.Y.Z-rc.N` tag push (beta track).
- `release.yml` — triggered on any other `v*` tag push (signed stable).

All CI runs on a self-hosted runner. No build/lint/test CI runs on ordinary
commits or PRs to `main` beyond the dev-track workflow above.

## Don't
- Don't embed credentials in remote URLs — SSH or a credential helper only.
- Don't invent bread command verbs that have no real breadcrumbs feature
  behind them. See EVENTS.md.
