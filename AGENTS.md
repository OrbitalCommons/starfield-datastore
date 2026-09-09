# AGENTS.md

Guidance for anyone — human or agent — working in this repository.

## What this is

`starfield-datastore` is a pull-through artifact cache plus the ephemeris
server that fronts it. Read `docs/spec.md` before changing anything; it is the
authoritative design and carries a decisions log. Open questions and design
discussion live on the issue tracker; a decision is not made until it is in
the spec's log.

## Hard rules

- **Never depend on `starfield`.** `starfield` depends on this crate, so the
  reverse is circular. No starfield types in the public API: std, `url`,
  `PathBuf` only. No nalgebra. This crate knows nothing about SPICE or any
  other data format; format checks are constructed by the consumer.
- **Validation fails closed.** Every artifact has a `ContentCheck`; the
  default is `All([NotHtml, MinBytes(1024)])`. `ContentCheck::None` must be
  spelled to be used. A check's job is to reject the wrong *kind* of thing
  (an HTML login page under a `.bsp` key); the consumer's parser validates the
  format. When in doubt, loosen — a false rejection has no backstop.
- **Credentials never enter the cache** — not the blob, not the index sidecar,
  not the key. The index records a provider *identity*, never a secret.
  Redirects drop credentials unless `Source::trust_redirects`. `Secret` has no
  `Debug`/`Display` that reveals its value and is zeroed on drop.
- **Only the ephemeris server writes to the mirror.** Client-side upstream
  fetches under `STARFIELD_ALLOW_UPSTREAM` populate the local cache only.
- **Manifests are declarative.** `ContentCheck::custom` holds a closure and
  cannot appear in TOML; manifests round-trip the declarative set only.

## Testing

- **No test in the default suite touches the network.** Mirror and upstream
  integration tests are `#[ignore]`d with a reason that names what they touch,
  e.g. `#[ignore = "live upstream; bypasses the mirror"]`.
- **Upstream canaries fail, never skip.** A live test that cannot reach
  upstream must fail naming `STARFIELD_ALLOW_UPSTREAM`. Do not write
  `if env::var(..).is_err() { return; }` — a canary that passes forever is
  worse than none.
- Credential rules and `Mirror::Http` redirect behaviour are unit-tested with
  `StaticProvider` and a local HTTP stub.
- `cargo publish --dry-run` runs in CI so the crate is known to package
  standalone.

## Build

```
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Run `cargo fmt` first, then fix any clippy findings, before every commit.

## Conventions

- Branches are `meawoppl/<dashed-description>`; never commit directly to
  `main`.
- Commit titles are ten words or fewer. No attribution to AI tools anywhere —
  commits, PR bodies, or code. CI enforces this.
- Add files individually; never `git add -A`.
- No dead code, no comments about removed code, no backward-compatibility
  shims unless asked, no scripts that edit code.
- Additive changes ship as patch versions. A minor bump is a breaking change
  and says so in `CHANGELOG.md`; every version bump updates `CHANGELOG.md`.
- Publish to crates.io from a clean checkout of `main` and tag `vX.Y.Z`.
  Downstream repositories consume this crate by version, never by git rev.
