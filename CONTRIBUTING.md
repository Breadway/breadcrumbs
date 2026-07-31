# Contributing

`breadcrumbs` — Profile-aware Wi-Fi state machine with Tailscale integration.

Part of the bread ecosystem; this repo follows the same branch/release
workflow as every other ecosystem product.

## Branches

There is one long-lived branch: **`main`**. All day-to-day work lands here.
Every push to `main` automatically builds and publishes a **dev-track**
build (see Tracks below) — a real install you can test before cutting
anything more formal.

New work — features and bug fixes alike — goes on a short-lived branch:

```
feature/<short-name>
fix/<issue-number-or-short-name>
```

Branch off `main`, open a PR/push back into `main` when ready. Short-lived
branches get deleted on merge — they never accumulate the kind of drift a
second long-lived branch does.

## The release cycle

There's no separate `beta` or release branch — "stable" and "beta" are both
just **tags** on `main`, not branches that need to be kept in sync:

1. Work accumulates on `main` via `feature/x` / `fix/x` branches. Each push
   auto-publishes a dev build — install it with `bakery track set dev` and
   `bakery update --all`, then fix anything broken with another push.
2. When you want to stabilize before a real release, tag a release
   candidate: `git tag vX.Y.Z-rc.1 && git push origin vX.Y.Z-rc.1` (push to
   both remotes). That tag alone triggers a beta-track build —
   "freezing" is just pausing pushes to `main` while you test it, not a
   branch operation. Cut `-rc.2`, `-rc.3`, etc. for further fixes.
3. Once an RC has gone without issues, tag the real release:
   `git tag vX.Y.Z && git push origin vX.Y.Z` — that's what triggers the
   signed stable release build.

## Tracks, from a user's perspective

```
bakery track show              # what you're currently on (defaults to stable)
bakery track set dev           # or beta, or stable
bakery update --all            # pull the latest build on your current track
```

| Track  | What it is | Published from |
|--------|-----------|-----------------|
| `stable` | The last tagged release | a `vX.Y.Z` tag |
| `beta` | Latest release candidate | a `vX.Y.Z-rc.N` tag |
| `dev` | Bleeding edge | `main`, on every push |

Dev versions are auto-computed (`X.Y.Z-dev.<timestamp>+<sha>`) from the
latest published stable tag, so they always sort as newer than what you
have installed — no manual version bumping needed. Beta versions are just
the RC tag itself (already valid semver, already sorts below the real
release it's a candidate for).

## Local development

```sh
cargo build --release
cargo test --release
```

## CI

- `dev-release.yml` — triggered on push to `main`.
- `rc-release.yml` — triggered on any `vX.Y.Z-rc.N` tag push.
- `release.yml` — triggered on any other `v*` tag push, cuts the actual
  stable release.

All CI runs on a self-hosted runner; nothing runs automatically on plain
commits or PRs beyond the track builds above. See
[bread-ecosystem's docs/release-channels.md](https://git.breadway.dev/Breadway/bread-ecosystem/src/branch/main/docs/release-channels.md)
for the full policy, including how a new product gets wired onto these tracks.

## Questions

Open an issue on this repo's Forgejo tracker.
