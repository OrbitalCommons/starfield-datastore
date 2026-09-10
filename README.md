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

Rollout steps 1–4 of `docs/spec.md` §15 are implemented here: identity and
validation, credentials, manifests, the content-addressed local store and
resolution chain, the S3 mirror, and the `starfield-datastore` binary.
`manifests/ephemeris.toml` lists the kernels `starfield` needs and
`docs/deploy.md` describes running the server. Step 5 — routing
`starfield::Loader` and the `starfield-datasources` downloaders through this
crate — lives in those repositories.

## Library

```rust,no_run
use starfield_datastore::{Artifact, ArtifactKey, ContentCheck, Datastore, Source};

# fn main() -> starfield_datastore::Result<()> {
let artifact = Artifact::new(
    ArtifactKey::new("archive/example.bin")?,
    vec![Source::new("https://archive.example/example.bin")],
).with_check(ContentCheck::default_binary());

let store = Datastore::from_env()?;
let path = store.get(&artifact)?;
# Ok(())
# }
```

The default chain is local disk, then the configured mirror. Set
`STARFIELD_ALLOW_UPSTREAM=1` to permit archive downloads, or
`STARFIELD_OFFLINE=1` to use only local files. `STARFIELD_CACHE_DIR` overrides
`~/.cache/starfield`; `STARFIELD_MIRROR` names the ephemeris server base URL.
The config file is `~/.config/starfield/datastore.toml` (XDG paths are honored).

`Datastore::builder()` uses explicit settings and defaults;
`DatastoreBuilder::from_env()` loads environment/file settings before explicit
builder overrides. The synchronous API returns content-addressed paths.
Built-in validation and SHA-256 checks use bounded memory; custom predicates
load the complete body. `get_bytes()` is for small files only.

Every `get()` rehashes and validates cached content. Corruption fails loudly;
repair is explicit. `import(&artifact, &path)` validates and copies a legacy
cache file or manually obtained artifact into the store. With no sources,
`provenance.description` explains how to obtain missing data. Progress can be
reported through `on_progress` instead of the default terminal bar.

Returned paths remain valid until an explicit `remove` or `gc`. Never run
those operations while consumers retain paths. The cache budget is advisory
until `gc` is run; fetching never evicts another consumer's data. Interrupted
downloads discard their temporary file; range-resume is not implemented.

## Commands and server

Build local commands with `cargo build --features cli`, or include S3 and the
ephemeris service with `cargo build --features server`. The library's default
features are `mirror-http` and `progress`; AWS dependencies are opt-in.

```sh
starfield-datastore fetch --manifest manifests/ephemeris.toml --key naif/spk/de421.bsp
starfield-datastore import --manifest manifest.toml --key manual/library --from library.dat
starfield-datastore list --bytes
starfield-datastore remove --key manual/library
starfield-datastore verify --manifest manifests/ephemeris.toml
starfield-datastore gc --max-bytes 1000000000

starfield-datastore mirror --manifest manifests/ephemeris.toml --to s3://BUCKET/ephemeris --region us-west-2
starfield-datastore serve --manifest manifests/ephemeris.toml --bucket s3://BUCKET/ephemeris --region us-west-2
starfield-datastore verify --manifest manifests/ephemeris.toml --at s3://BUCKET/ephemeris --region us-west-2
```

Only `mirror`, `serve`, and explicit S3 repair upload objects. Client fetches
never write the mirror, including when a `Mirror` has `writable: true`.
`mirror` writes digest pins back atomically and requires a declared license
before redistribution. S3 verification downloads and hashes the object;
`--repair` must be supplied to change corrupt content.

The service defaults to `127.0.0.1:8080` and accepts only loopback or Tailscale
bind addresses. Its manifest is the allow-list: unknown keys return 404.
`GET /artifact/<key>` returns a five-minute presigned S3 redirect; `/healthz`
reports process liveness. The bucket must remain private, and exposure must
remain within the tailnet.

Upstream credentials come from host-specific `STARFIELD_TOKEN_<HOST>` variables
(uppercase host with punctuation replaced by underscores), `~/.netrc`, or the
optional `onepassword` provider. `STARFIELD_OP_ITEM=op://vault/item`
selects an item whose fields are named for archive hosts. S3 uses the native
AWS credential chain, including session credentials. Source URLs cannot embed
userinfo. Redirect targets are validated; credential forwarding requires an
explicit trusted source, and mirror redirects receive no archive credentials.

Run synchronous datastore/S3 operations through `spawn_blocking` when embedding
them in an asynchronous application.

## Relationship to `starfield`

This crate **never depends on `starfield`**. The planned consumer integration
adds a default-on `datastore` feature to `starfield`, which builds SPICE-kernel
content checks from its own constants. Consumer repositories use crates.io
versions, never git revisions; their rollout is tracked separately.

## Development

The [container deployment guide](docs/deploy.md#10-container-image-and-one-file-configuration)
covers `ghcr.io/orbitalcommons/starfield-datastore`, automatically published on
every push to main after tests. One [configuration file](deploy/config.toml)
defines cache settings, named services, and lazy credential sources.

```
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo test --no-default-features
cargo clippy --all-targets --no-default-features --features cli -- -D warnings
cargo test --no-default-features --features server
```

No test in the default suite touches the network. Mirror and upstream
integration tests are `#[ignore]`d; the upstream ones fail — never skip — when
`STARFIELD_ALLOW_UPSTREAM` is unset. See `AGENTS.md`.

## License

MIT.
