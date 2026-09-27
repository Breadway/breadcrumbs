# Contributing

`breadcrumbs`: Profile-aware Wi-Fi state machine with Tailscale integration.

Part of the bread ecosystem; this repo follows the same branch/release
workflow as every other ecosystem product.

## Branches

There is one long-lived branch: **`main`**. All day-to-day work lands here.
Every push to `main` automatically builds and publishes a **dev-track**
build (see Tracks below): a real install you can test before cutting
anything more formal.

New work, features and bug fixes alike, goes on a short-lived branch:

```
feature/<short-name>
fix/<issue-number-or-short-name>
```

Branch off `main`, open a PR/push back into `main` when ready. Short-lived
branches get deleted on merge; they never accumulate the kind of drift a
second long-lived branch does.

## The release cycle

There's no separate `beta` or release branch: "stable" and "beta" are both
just **tags** on `main`, not branches that need to be kept in sync:

1. Work accumulates on `main` via `feature/x` / `fix/x` branches. Each push
   auto-publishes a dev build; install it with `bakery track set dev` and
   `bakery update --all`, then fix anything broken with another push.
2. When you want to stabilize before a real release, tag a release
   candidate: `git tag vX.Y.Z-rc.1 && git push origin vX.Y.Z-rc.1` (push to
   both remotes). That tag alone triggers a beta-track build;
   "freezing" is just pausing pushes to `main` while you test it, not a
   branch operation. Cut `-rc.2`, `-rc.3`, etc. for further fixes.
3. Once an RC has gone without issues, tag the real release:
   `git tag vX.Y.Z && git push origin vX.Y.Z`, that's what triggers the
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
have installed, no manual version bumping needed. Beta versions are just
the RC tag itself (already valid semver, already sorts below the real
release it's a candidate for).

## Local development

```sh
cargo build --release
cargo test --release
```

Cargo.toml pins the `git.breadway.dev` dependency (bread-utils) to a
release tag; the commands above build against exactly that tagged
version, same as a stable/beta release would. To instead build against
its own `main` (what `dev-release.yml` and `check.yml` do, and what you
need to exercise a cross-repo change before either side is tagged), run
bread-ecosystem's shared script from this checkout:

```sh
/path/to/bread-ecosystem/scripts/dev-deps.sh --root .
cargo build --release   # no --locked: the patched dep resolves to main
```

This clones bread-ecosystem's `main` as a true sibling of this checkout
(or wherever `DEV_DEPS_DIR` points, if set), writes a generated,
gitignored `.cargo/config.toml`, and updates `Cargo.lock` to match. Discard
both (`git checkout -- Cargo.lock` and remove `.cargo/config.toml`) when
you're done. See bread-ecosystem's `docs/release-channels.md`
("Cross-repo dependency resolution") for the full model.

## CI

- `check.yml`: triggered on push to `feature/**`/`fix/**`. Lints
  git.breadway.dev dependency pins, then builds/clippies/tests in the same
  dev mode as `dev-release.yml` (see above), so a PR that needs a
  just-merged, not-yet-tagged dependency change still passes.
- `dev-release.yml`: triggered on push to `main`. Also dev mode: resolves
  the dependency to its own `main` before building, unlike the two tracks
  below.
- `rc-release.yml`: triggered on any `vX.Y.Z-rc.N` tag push. Builds
  against the tagged Cargo.toml pin with `--locked`; no dependency
  patching.
- `release.yml`: triggered on any other `v*` tag push, cuts the actual
  stable release. Same `--locked`, tag-pinned build as `rc-release.yml`.

All CI runs on a self-hosted runner. See
[bread-ecosystem's docs/release-channels.md](https://git.breadway.dev/Breadway/bread-ecosystem/src/branch/main/docs/release-channels.md)
for the full policy, including how a new product gets wired onto these tracks.

## Questions

Open an issue on this repo's Forgejo tracker.
