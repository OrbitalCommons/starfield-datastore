# starfield-datastore

A pull-through artifact cache and the ephemeris server that fronts it, for the
OrbitalCommons data stack. Every crate that needs a large, named, mostly
immutable data file — a SPICE kernel, a star-catalogue shard, a PDS table, a
planetary mosaic — fetches it through this.

A request resolves down a chain, populating the nearer layers as it goes:

```
local disk  →  ephemeris server (tailnet only; 302 → presigned S3 URL)  →  upstream archive
                                                                            (only with STARFIELD_ALLOW_UPSTREAM=1)
```

Three properties the design exists to guarantee:

- **A checkout depends on one service we control**, not on NAIF, PDS, LP DAAC
  and USGS all being up at once.
- **Nothing wrong is ever cached.** Archives answer HTTP 200 with a login page
  or a catalogue page; validation is mandatory and fails closed, so an HTML
  body never lands under a `.bsp` key — locally or in the mirror.
- **Upstream credentials live in exactly one place**: the ephemeris server.
  Nobody else in the organisation needs an account on any archive.

The design, API, and the log of decisions behind them are in
[`docs/spec.md`](docs/spec.md). Design discussion continues on the transferred
issue, [#1](https://github.com/OrbitalCommons/starfield-datastore/issues/1)
(formerly `starfield#189`).

## Status

Specification complete; implementation not started. Rollout order is in
`docs/spec.md` §15: skeleton → credentials → manifests → S3 layer and server →
ephemeris manifest and the consumer shims.

## Relationship to `starfield`

This crate **never depends on `starfield`**; `starfield` depends on it, behind
a default-on `datastore` feature, and builds the SPICE-kernel content checks
from its own constants. `starfield-datasources` and `focalplane` depend on it
directly, by crates.io version — never by git rev.

## Development

```
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

No test in the default suite touches the network. Mirror and upstream
integration tests are `#[ignore]`d; the upstream ones fail — never skip — when
`STARFIELD_ALLOW_UPSTREAM` is unset. See `AGENTS.md`.

## License

MIT.
