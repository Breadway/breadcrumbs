# Contributing

`breadcrumbs` — Profile-aware Wi-Fi state machine with Tailscale integration.

Part of the bread ecosystem; this repo follows the same branch/release
workflow as every other ecosystem product.

## Branches

- **`main`** — release branch, always tag-ready. Nothing is committed to it
  directly; it only moves forward via a `beta` merge (see below).
- **`dev`** — integration branch. All day-to-day work lands here first.
  Every push to `dev` automatically builds and publishes a **dev-track**
  build (see Tracks below) — use this to test your change in a real install
  before it goes any further.
- **`beta`** — a frozen stabilization branch, cut from `dev` periodically.
  Every push to `beta` automatically builds and publishes a **beta-track**
  build. While a freeze is active, only fixes for issues found *in that
  freeze* should land on `beta`.

New work — features and bug fixes alike — goes on a short-lived branch:

```
feature/<short-name>
fix/<issue-number-or-short-name>
```

Branch off `dev`, open a PR/push back into `dev` when ready. If you're fixing
something reported against an active `beta` freeze, branch off `beta`
instead, merge the fix there to unblock testers, and also forward the same
fix into `dev` so it doesn't quietly reappear next cycle.

## The release cycle

1. Work accumulates on `dev` via `feature/x` / `fix/x` branches. Each push
   auto-publishes a dev build — install it with `bakery track set dev` and
   `bakery update --all`, then report or fix anything broken with another
   push to `dev`.
2. Once `dev` has gone roughly **a week** without new issues, `beta` is cut
   fresh from `dev`'s current tip. This freezes it as the stabilization
   target — `dev` keeps moving independently starting the next cycle.
3. `beta` is open for anyone to test: `bakery track set beta` and
   `bakery update --all`. **File issues against anything you find on this
   repo's Forgejo issue tracker.** Fixes land via `fix/<issue>` branches
   merged into `beta`.
4. Once `beta` has gone roughly **a month** without new issues, it's merged
   into `main` and tagged `vX.Y.Z` — that tag is what actually triggers the
   stable release build. `beta` is then reset from `dev` to start the next
   cycle.

## Tracks, from a user's perspective

```
bakery track show              # what you're currently on (defaults to stable)
bakery track set dev           # or beta, or stable
bakery update --all            # pull the latest build on your current track
```

| Track  | What it is | Published from |
|--------|-----------|-----------------|
| `stable` | The last tagged release | `main`, on a `vX.Y.Z` tag push |
| `beta` | Current stabilization freeze | `beta`, on every push |
| `dev` | Bleeding edge | `dev`, on every push |

Dev/beta versions are auto-computed (`X.Y.Z-dev.<timestamp>+<sha>` /
`-beta.…`) from the latest published stable tag, so they always sort as
newer than what you have installed — no manual version bumping needed when
pushing to `dev` or `beta`.

## Local development

```sh
cargo build --release
cargo test --release
```

## CI

- `dev-release.yml` — triggered on push to `dev`.
- `beta-release.yml` — triggered on push to `beta`.
- `release.yml` — triggered on a `v*` tag push, cuts the actual stable release.
- `package.yml` — publishes to the `[breadway]` pacman repo, also tag-triggered.

All CI runs on a self-hosted runner; nothing runs automatically on plain
commits or PRs beyond the track builds above. See
[bread-ecosystem's docs/release-channels.md](https://git.breadway.dev/Breadway/bread-ecosystem/src/branch/main/docs/release-channels.md)
for the full policy, including how a new product gets wired onto these tracks.

## Questions

Open an issue on this repo's Forgejo tracker.
