# starfield-datastore — specification

A pull-through artifact cache and the ephemeris server that fronts it. Every
crate in the organisation fetches large, named, mostly immutable data files
through it: SPICE kernels, star-catalogue shards, PDS tables, planetary
mosaics. A request resolves down a chain — local disk, then the organisation's
mirror, then (only when explicitly allowed) the upstream archive — populating
the nearer layers as it goes.

This document consolidates the design issue — now [#1](https://github.com/OrbitalCommons/starfield-datastore/issues/1),
transferred from `OrbitalCommons/starfield#189` — and the decisions recorded in its
comments. It is the authoritative description of the crate that lives in this
repository. The prose rationale and the resolution-chain diagram are in
`OrbitalCommons/starfield-datasources#72` and that repository's
`docs/datastore-spec.md`.

## 1. Goals

1. **A checkout depends on one service we control.** Not on NAIF, PDS, LP DAAC
   and USGS all being up at once.
2. **Nothing wrong is ever cached.** Archives return HTTP 200 for failures —
   USGS serves a catalogue page for an unknown product slug, LAADS serves an
   HTML login page to an unauthenticated download. A cache that stores a login
   page under a `.hdf` key converts a loud, immediate failure into silent,
   persistent corruption that survives restarts and propagates to the mirror.
   Validation is mandatory and fails closed by default.
3. **Credentials live in one place.** Only the ephemeris server holds
   upstream credentials. No one else in the organisation needs an account on
   any archive, and nothing secret is ever synced anywhere.
4. **No service to keep running for reads.** Once mirrored, the bytes are in
   S3; the server is in the control path only.

### Not in scope

- Not a general HTTP cache. Named artifacts only; HORIZONS, SBDB and broker
  responses are query-shaped and stay as they are.
- Not a package manager.
- Not a substitute for the archives. Every artifact records where it came from
  so it can be re-derived.
- Not a vault. The datastore does not hand out upstream secrets; it removes
  the need for anyone but the server to have them.

## 2. Topology

```
                tailnet only
 client ──────────────────────▶ ephemeris server ──▶ S3 (private bucket)
   │  1. GET /artifact/<key>        │  holds all upstream credentials
   │  2. 302 → presigned S3 URL     │  sole IAM identity (writer role)
   │◀────────────────────────────── │  fills misses from upstream
   │  3. GET presigned URL          ▼
   └────────────────────────────▶ S3 (bytes flow direct; server never carries them)

 off-tailnet, STARFIELD_ALLOW_UPSTREAM=1:
 client ──▶ (server unreachable) ──▶ upstream archive, caller's own credentials
```

### 2.1 Access control: a network boundary, not user auth

The ephemeris server is reachable only on the organisation's tailnet
(Tailscale). Nothing in the mirror is sensitive; the boundary exists so the
service cannot be abused from the open internet. The datastore implements no
user database and no per-user tokens.

### 2.2 Read path: presigned redirects

The bucket is private with Block Public Access on. On a hit the server answers
**302 → S3 presigned GET URL** (SigV4 query-string auth, expiry 5–15 minutes)
and the client downloads directly from S3. Anonymous download is impossible: no
public bucket policy exists, a URL is obtainable only from inside the tailnet,
and it expires in minutes. The residual exposure — a presigned URL is a
short-lived bearer capability — is acceptable for public-domain artifacts.

Consequence for the client: `Mirror::Http` must follow a cross-host 302 and
must **not** forward the mirror request's credentials to the redirect target.
That is the existing redirect rule (§6), and a presigned URL carrying its own
auth is exactly the case it was written for. No new `Mirror` variant is needed.

### 2.3 Write path: the server is the only writer

On a miss the server fetches upstream with its credentials, validates (§5),
writes through to S3 with a conditional put, then redirects as above. A
scheduled `mirror` batch job (§10) runs on the server to pre-warm the manifest;
on-demand fill covers what it has not reached yet.

Upstream fetches made by a client under `STARFIELD_ALLOW_UPSTREAM` populate
that client's local cache only. They never write to the mirror: such a fetch
happens with whatever credentials the caller has, against whatever the archive
serves that day, with no server-side validation in the path. Local-only keeps
the blast radius of a misconfigured fetch at one machine instead of making one
developer's HTML login page everyone's cached artifact.

### 2.4 Off-tailnet: opt-in upstream fallback

Default chain: `local disk → ephemeris server`. A miss with the server
unreachable is a loud error naming the variable below.

`STARFIELD_ALLOW_UPSTREAM=1` adds a third layer: `local disk → ephemeris
server → upstream`, fetching from the archives with whatever credentials the
caller has. Off by default. An outside contributor or a GitHub-hosted CI runner
sets one variable and can run the suite.

Not chosen, recorded so they are not rediscovered: a second public-read bucket
of the public-domain subset (can be added later without changing the client
API), and a strict no-fallback boundary.

### 2.5 Deferred: raw S3 from the tailnet

If the server ever becomes a read-path bottleneck, or the read path must
survive the server being down: an S3 interface endpoint in a VPC, a Tailscale
subnet router, and a bucket policy allowing `s3:GetObject` to `Principal: "*"`
conditioned on `aws:SourceVpce`. Raw S3 URLs then work only from the tailnet
and are a 403 from anywhere else; AWS treats such a policy as non-public, so
Block Public Access stays on. Not now.

## 3. Placement and dependency direction

The datastore is its own repository and crate:
`OrbitalCommons/starfield-datastore`, published to crates.io as
`starfield-datastore`. It sits *below* `starfield` in the dependency graph,
carries a service binary and its deployment alongside the library, shares no
types with `starfield`, and releases on an ops cadence rather than a feature
cadence — none of which belongs inside an astronomy library's tree.

**`starfield-datastore` must not depend on `starfield`.** `starfield` uses it
for kernel loading, so the reverse would be circular. Consequences:

- Its own `DatastoreError`; `starfield` adds a `Datastore(#[from]
  DatastoreError)` variant to `StarfieldError` behind an optional, default-on
  `datastore` feature.
- No starfield types in its public API: std, `url`, `PathBuf` only. No
  nalgebra, no starfield time types.
- It knows nothing about SPICE or any other format (§5.2). The kernel checks
  and the `Loader` seam live in `starfield`.

Release conditions:

- Published to crates.io from its first release. `starfield` depends on it as
  `{ version = "0.1", optional = true }`; `starfield-datasources` and
  `focalplane` depend on it directly, by version. Never by git rev: two
  starfield versions in one build already broke the shared catalogue traits
  once, and at the layer everything depends on it would be worse.
- Additive changes ship as patch versions; a minor bump is a breaking change
  and says so in `CHANGELOG.md`.
- Publish `starfield-datastore` before the `starfield` release that first
  requires the new version.

## 4. Identity

```rust
/// Stable logical key. Archive-shaped, never a URL: LP DAAC relocated twice
/// this year, and that must change a Source rather than the cache layout,
/// the mirror object path, or anyone's pinned digest.
///
/// Slash-separated, no leading slash, no `..`. e.g. "naif/spk/de440.bsp".
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArtifactKey(String);

impl ArtifactKey {
    pub fn new(key: impl Into<String>) -> Result<Self>;   // validates shape
    pub fn as_str(&self) -> &str;
    /// Object path under a mirror prefix.
    pub fn mirror_path(&self, prefix: &str) -> String;
}

/// Where to obtain an artifact upstream.
#[derive(Debug, Clone)]
pub struct Source {
    pub url: String,
    /// Forward credentials across redirects. Required for Earthdata URS.
    /// Off by default: forwarding a bearer token to an arbitrary redirect
    /// target is a credential-exfiltration primitive.
    pub trust_redirects: bool,
}

impl Source {
    pub fn new(url: impl Into<String>) -> Self;           // trust_redirects = false
    pub fn trusting_redirects(self) -> Self;
}

#[derive(Debug, Clone)]
pub struct Provenance {
    pub description: String,
    /// SPDX id or free text. Mirroring is redistribution; assert per artifact.
    pub license: String,
    pub citation: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Artifact {
    pub key: ArtifactKey,
    pub sources: Vec<Source>,
    pub check: ContentCheck,
    pub freshness: Freshness,
    pub provenance: Provenance,
    /// Known size, for progress reporting and a cheap sanity check.
    pub expected_bytes: Option<u64>,
}

impl Artifact {
    pub fn new(key: ArtifactKey, sources: Vec<Source>) -> Self;  // check = default_binary()
    pub fn with_check(self, check: ContentCheck) -> Self;
    pub fn with_freshness(self, freshness: Freshness) -> Self;
    pub fn with_provenance(self, provenance: Provenance) -> Self;
    pub fn with_expected_bytes(self, bytes: u64) -> Self;
}
```

## 5. Validation

### 5.1 `ContentCheck`

```rust
pub enum ContentCheck {
    /// Best. Lowercase hex SHA-256.
    Sha256(String),
    /// Leading bytes match any prefix; see `magic`.
    Magic { prefixes: Vec<Vec<u8>>, trim_leading_whitespace: bool },
    MinBytes(u64),
    /// Reject an HTML body where a binary was expected. Catches soft-404s and
    /// soft-auth-walls, which are the same shape of failure.
    NotHtml,
    /// Arbitrary predicate returning a reason on failure. Code-only.
    Custom(Arc<dyn Fn(&[u8]) -> std::result::Result<(), String> + Send + Sync>),
    All(Vec<ContentCheck>),
    /// Explicit opt-out. Must be spelled to be used.
    None,
}

impl ContentCheck {
    /// Accept if the leading bytes match any prefix.
    ///
    /// `trim_leading_whitespace` is required for text kernels, which may open
    /// with blank lines; it must be false for binary formats, where the
    /// signature is at a fixed offset and leading whitespace is already wrong.
    pub fn magic(prefixes: Vec<Vec<u8>>, trim_leading_whitespace: bool) -> Self;
    pub fn custom(f: Arc<dyn Fn(&[u8]) -> std::result::Result<(), String> + Send + Sync>) -> Self;
    /// `All([NotHtml, MinBytes(1024)])`. The default for every artifact, so the
    /// soft-404 and soft-auth-wall cases fail closed without anyone remembering
    /// they exist.
    pub fn default_binary() -> Self;
    pub fn check(&self, bytes: &[u8]) -> std::result::Result<(), CheckFailure>;
}

#[derive(Debug, Clone)]
pub struct CheckFailure {
    pub check: String,
    /// Summary of what arrived: status, content-type, first printable bytes.
    /// When the body looks like an HTML login form, say so explicitly —
    /// that is the most common real cause.
    pub got: String,
}
```

`Custom` holds a closure, so `ContentCheck` does not derive `Debug`,
`PartialEq` or `Serialize`. `Debug` is hand-written and prints
`Custom(<predicate>)`. Manifests (§9) express the declarative variants only —
`Sha256`, `magic`, `MinBytes`, `NotHtml`, `All` — and round-trip through TOML;
`custom` cannot appear in a manifest.

### 5.2 The check rejects the wrong kind of thing; the parser validates the format

A check exists to catch an HTML login page or a catalogue soft-404 under a
`.bsp` key. The format validator is the consumer's parser, which runs
afterwards and is far better at it. The two failure modes are not symmetric: a
loose check still has `NotHtml`, `MinBytes` and — once mirrored — a SHA-256
beneath it; a tight check produces a false rejection with no backstop, refusing
data that is fine and handing the user an integrity error. **When in doubt,
loosen.**

The datastore therefore knows nothing about any format. `starfield` constructs
the kernel checks from its own constants at the call site, behind the
`datastore` feature, in the same table as `resolve_url` (extension → URL), so
there is one mapping to keep in sync:

| Extension | Check | Source of truth |
|---|---|---|
| `.tpc`, `.tf` | `magic(TEXT_MAGIC_NUMBERS, trim = true)` | `starfield::planetarylib::TEXT_MAGIC_NUMBERS` (`KPL/FK`, `KPL/PCK`) |
| `.bsp` | `magic(["DAF/SPK", "NAIF/DAF"], trim = false)` | `jplephem::daf` reads the ID word at bytes 0..8 without whitelisting |
| `.bpc` | `magic(["DAF/PCK", "NAIF/DAF"], trim = false)` | as above |

`NAIF/DAF` is the pre-N0052 ID word and still appears in older archived
kernels; starfield parses such files today, so the cache must not reject them.
The DAF ID word is at a fixed offset, so a prefix check is exact and no `DAF`
need be constructed.

### 5.3 Freshness

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Freshness {
    /// Never re-fetch. Almost everything: a numbered SPICE kernel, a specific
    /// PDS volume file, a published mosaic. What makes mirroring safe.
    Immutable,
    /// MPCORB, broker snapshots.
    Ttl(std::time::Duration),
    /// Conditional GET with ETag / Last-Modified.
    Revalidate,
}
```

Ship `Immutable` only through rollout step 4; add `Ttl` and `Revalidate` when
an artifact needs them.

## 6. Credentials

```rust
/// A secret that cannot be logged by accident.
/// No Debug/Display that reveals the value, no Serialize, zeroed on drop.
pub struct Secret(/* private */);

impl Secret {
    pub fn new(value: String) -> Self;
    /// The only way out. Deliberately awkward to call.
    pub fn expose(&self) -> &str;
}
impl std::fmt::Debug for Secret { /* writes "Secret([redacted])" */ }

#[derive(Clone)]
pub enum Credential {
    Basic { user: String, secret: Secret },
    Bearer { secret: Secret, expires_at: Option<SystemTime> },
    AwsSigV4 { access_key: String, secret: Secret, region: String },
}

pub trait CredentialProvider: Send + Sync {
    /// Credential for the host of the request actually being made, or None.
    fn credential_for(&self, host: &str) -> Result<Option<Credential>>;
    /// Identity used, for the manifest. Never the secret.
    fn identity_for(&self, host: &str) -> Option<String>;
}

pub struct NetrcProvider;          // ~/.netrc — what Earthdata URS wants
pub struct EnvProvider;            // STARFIELD_TOKEN_<HOST>
pub struct OnePasswordProvider { pub item: String }   // `op read`, never to disk
pub struct AwsProvider;            // standard AWS chain, for the S3 mirror
pub struct StaticProvider(pub HashMap<String, Credential>);  // tests
pub struct ChainProvider(pub Vec<Box<dyn CredentialProvider>>);
```

Where they run: the provider chain is **server-side configuration**. Clients
need no provider at all on the tailnet, and only their own archive credentials
under `STARFIELD_ALLOW_UPSTREAM`.

Rules the implementation must hold, all testable:

1. Credentials never enter the cache — not the blob, not the sidecar, not the
   key. The manifest records the provider identity, never the secret.
2. Redirects drop credentials unless `Source::trust_redirects`. This is also
   what makes presigned redirects safe (§2.2).
3. "No credential configured" and "credential rejected" are distinct error
   variants. They are different problems for the user.
4. Lookup is keyed on the host of the request actually being made, not the
   artifact's host. Earthdata redirects `ladsweb…` and
   `data.lpdaac.earthdatacloud.nasa.gov` through `urs.earthdata.nasa.gov`;
   `.netrc` semantics already work this way and so must the trait.
5. A `Bearer` credential may carry an expiry (Earthdata tokens last ~60 days,
   MAST tokens likewise). `CredentialRejected` reports whether the credential
   looked expired, so the fix is obvious.

## 7. The store

```rust
pub struct Datastore { /* private */ }
pub struct DatastoreBuilder { /* private */ }

impl Datastore {
    pub fn builder() -> DatastoreBuilder;
    /// From env + ~/.config/starfield/datastore.toml + defaults.
    pub fn from_env() -> Result<Self>;

    /// Resolve to a local path, fetching through the chain if needed.
    pub fn get(&self, artifact: &Artifact) -> Result<PathBuf>;
    /// As `get`, reporting which layer served it.
    pub fn get_with_outcome(&self, artifact: &Artifact) -> Result<(PathBuf, ResolveOutcome)>;
    /// Local path if already cached and fresh; never fetches.
    pub fn peek(&self, key: &ArtifactKey) -> Option<PathBuf>;
    /// Read fully into memory. For small artifacts only.
    pub fn get_bytes(&self, artifact: &Artifact) -> Result<Vec<u8>>;

    pub fn contains(&self, key: &ArtifactKey) -> bool;
    pub fn remove(&self, key: &ArtifactKey) -> Result<()>;
    pub fn keys(&self) -> Result<Vec<ArtifactKey>>;
    pub fn total_bytes(&self) -> Result<u64>;
    /// Rehash every blob; report keys whose content no longer matches.
    pub fn verify(&self) -> Result<Vec<VerifyFailure>>;
}

impl DatastoreBuilder {
    pub fn cache_root(self, path: PathBuf) -> Self;
    pub fn mirror(self, mirror: Mirror) -> Self;
    pub fn credentials(self, provider: Box<dyn CredentialProvider>) -> Self;
    /// Permit the upstream layer. Default false; see §2.4.
    pub fn allow_upstream(self, allow: bool) -> Self;
    /// Disable layers 2 and 3. What CI sets once the mirror is warm.
    pub fn offline(self, offline: bool) -> Self;
    pub fn timeout(self, timeout: Duration) -> Self;
    pub fn progress(self, enabled: bool) -> Self;
    pub fn build(self) -> Result<Datastore>;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Layer { LocalDisk, Mirror, Upstream }

#[derive(Debug, Clone)]
pub struct ResolveOutcome {
    pub layer: Layer,
    pub bytes: u64,
    pub duration: Duration,
    /// Which Source index succeeded, when layer == Upstream.
    pub source_index: Option<usize>,
}

#[derive(Debug, Clone)]
pub enum Mirror {
    /// Plain HTTPS GET against a base URL; follows cross-host redirects
    /// without forwarding credentials. This is what clients use for the
    /// ephemeris server.
    Http { base_url: String, writable: bool },
    /// Requires the `mirror-s3` feature. This is what the server uses.
    S3 { bucket: String, prefix: String, region: String, writable: bool },
}
```

Resolution order and the three modes:

| Mode | Chain | Who |
|---|---|---|
| default | local → mirror | everyone on the tailnet |
| `allow_upstream` | local → mirror → upstream | off-tailnet, CI, live rot tests |
| `offline` | local | CI once the cache is warm |

A miss in default mode with the mirror unreachable fails with
`MirrorUnreachable`, which names `STARFIELD_ALLOW_UPSTREAM`.

## 8. Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum DatastoreError {
    #[error("no credential configured for host {host}")]
    NoCredential { host: String },
    #[error("credential rejected by {host} (HTTP {status}){}", if *.looks_expired { "; it looks expired" } else { "" })]
    CredentialRejected { host: String, status: u16, looks_expired: bool },
    #[error("content rejected for {key}: failed {}; got {}", .failure.check, .failure.got)]
    ContentRejected { key: ArtifactKey, failure: CheckFailure },
    #[error("offline, and {key} is not in the local cache")]
    OfflineMiss { key: ArtifactKey },
    #[error("{key} is not cached and the mirror is unreachable; set STARFIELD_ALLOW_UPSTREAM=1 to fetch from the archive")]
    MirrorUnreachable { key: ArtifactKey },
    #[error("all {} sources failed for {key}", .attempts.len())]
    AllSourcesFailed { key: ArtifactKey, attempts: Vec<String> },
    #[error("invalid artifact key: {0}")]
    InvalidKey(String),
    #[error("mirror error: {0}")]
    Mirror(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

pub type Result<T> = std::result::Result<T, DatastoreError>;
```

## 9. Manifests

Simultaneously the pin, the mirror build input and the offline allow-list.

```toml
[[artifact]]
key = "naif/spk/de440.bsp"
sources = ["https://naif.jpl.nasa.gov/pub/naif/generic_kernels/spk/planets/de440.bsp"]
sha256 = "..."           # optional until first mirrored
bytes = 119857024
freshness = "immutable"
check = { magic = ["DAF/SPK", "NAIF/DAF"] }
description = "NAIF generic kernels, DE440 planetary ephemeris"
license = "public-domain"
```

```rust
pub struct Manifest { pub artifacts: Vec<Artifact> }

impl Manifest {
    pub fn from_toml_str(s: &str) -> Result<Self>;
    pub fn from_path(path: &Path) -> Result<Self>;
    pub fn to_toml_string(&self) -> Result<String>;
    pub fn get(&self, key: &ArtifactKey) -> Option<&Artifact>;
    pub fn merge(&mut self, other: Manifest);
}
```

**Digests are aspirational, not mandatory.** Most of these products publish no
checksum and computing one means downloading ~25 GB. Ship `magic` /
`NotHtml` / `MinBytes` immediately for everything; promote an artifact to
`Sha256` once it has been fetched and mirrored once, at which point the mirror
is the source of the digest. `mirror` writes the digest back into the manifest.

## 10. The ephemeris server and the CLI

Binary `starfield-datastore`:

```
starfield-datastore serve  --manifest M --bucket s3://bucket/prefix   # the ephemeris server
starfield-datastore fetch  --manifest M --key K                       # populate local cache
starfield-datastore mirror --manifest M --to s3://bucket/prefix       # batch pre-warm; runs on the server
starfield-datastore verify --manifest M [--at s3://...] [--repair]
starfield-datastore list   [--bytes]
starfield-datastore gc     --max-bytes N
```

`serve` implements §2: `GET /artifact/<key>` → 302 to a presigned URL, filling
misses from upstream with the server's credential chain first. `mirror` walks
the manifest, fetches upstream, validates, uploads and writes digests back; it
is scheduled on the server and is what makes the mirror warm before anyone
asks. Writes use S3 conditional puts (`If-None-Match: *`) so a batch run and an
on-demand fill of the same object resolve cleanly; content addressing makes
the loser's work harmless.

`verify --repair` is always explicit. Silently re-fetching on a digest
mismatch would hide a real problem.

## 11. Configuration

Resolved from: explicit builder call, then env, then
`~/.config/starfield/datastore.toml`, then defaults.

| Setting | Env | Default |
|---|---|---|
| Cache root | `STARFIELD_CACHE_DIR` | `~/.cache/starfield` |
| Mirror | `STARFIELD_MIRROR` | none (the org config sets the ephemeris server URL) |
| Allow upstream | `STARFIELD_ALLOW_UPSTREAM` | false |
| Offline | `STARFIELD_OFFLINE` | false |
| Max cache bytes | `STARFIELD_CACHE_MAX` | unbounded |

The cache root stays `~/.cache/starfield`, shared with the existing
`data::downloader`, so nobody re-downloads 114 MB on the day of the switch.

## 12. Storage layout

```
~/.cache/starfield/
  blobs/<aa>/<sha256>          # content-addressed, immutable once written
  index/<key path>.json        # { digest, bytes, fetched_at, source, etag, provider_identity }
  tmp/                         # in-flight; temp + fsync + rename
  locks/<key hash>.lock        # advisory, so two processes do not both pull 12 GB
```

Content addressing gives dedup, atomic publication, free integrity checking
and safe concurrent fetch. Blobs are written to `tmp/`, validated, then
renamed — a reader never observes a partial file, and two processes racing the
same artifact both end at the same final path.

## 13. Feature flags

| Feature | Default | Effect |
|---|---|---|
| `mirror-http` | yes | Plain HTTPS mirror transport (clients) |
| `mirror-s3` | no | `aws-sdk-s3` and presigning; heavy, so opt-in (the server) |
| `onepassword` | no | `op` CLI provider |
| `progress` | yes | `indicatif` bars |

## 14. Testing rules

- No test in the default suite touches the network. Mirror and upstream
  integration tests are `#[ignore]`d behind a live-network marker.
- The live upstream-rot tests stay pointed at the archives, not the mirror:
  they exist to detect that a pinned USGS slug still resolves or that LP DAAC
  has not relocated again, and a mirror would happily keep serving a copy
  whose upstream died two years ago. They set `STARFIELD_ALLOW_UPSTREAM` and
  label the exception in the ignore reason, e.g.
  `#[ignore = "live upstream; bypasses the mirror"]`.
- A canary that cannot reach upstream **fails, naming the variable — it never
  returns early.** The idiom `if env::var(..).is_err() { return; }` would let
  `cargo test -- --ignored` without the variable pass a suite that checked
  nothing, and a canary that passes forever is worse than no canary.
- The three credential rules (§6) and the redirect behaviour of
  `Mirror::Http` are unit-tested with `StaticProvider` and a local HTTP stub.
- Manifest round-trip is tested for the declarative `ContentCheck` set.

## 15. Rollout

Each step independently useful.

1. **Skeleton** — `Artifact`, `ContentCheck`, `Freshness::Immutable`, local
   layer, content-addressed store. No credentials or mirror. Fixes the
   soft-404 / soft-auth-wall class of bug immediately for existing callers via
   shims.
2. **Credentials** — providers, `Secret`, redirect policy, host-keyed lookup,
   expiry.
3. **Manifests + `verify`** — pins and digests for what is already downloaded.
4. **S3 layer + ephemeris server** — `Mirror::S3`, `serve` with presigned
   redirects and proxy-on-miss, batch `mirror`, `STARFIELD_ALLOW_UPSTREAM`,
   `STARFIELD_OFFLINE`, conditional puts.
5. **Ephemeris manifest and the shims** — the SPICE kernels `starfield` needs
   (`de421.bsp`, `de440.bsp`, `pck00011.tpc`, `moon_pa_de421_1900-2050.bpc`,
   `moon_080317.tf`, `naif0012.tls`); `starfield::Loader` resolves through the
   datastore, and **the nine `starfield-datasources` downloaders** (gaia,
   hipparcos, mast, nsa, planet-spectra, planet-maps, gaia-tools, …) do the
   same through the same seam. `starfield::data::downloader` and the
   datasources `download_to_file` / `cache_dir` become thin delegating shims,
   so no existing call site churns; the shims go when the last caller is
   converted. Any downloader deliberately left direct is documented as an
   escape hatch — a boundary that looks enforced and is not is worse than
   none.

## 16. Decisions log

| Date | Decision |
|---|---|
| 2026-09-09 | Proposed as a `starfield` workspace member. |
| 2026-09-09 | Own repository and crate, `OrbitalCommons/starfield-datastore`: it sits below starfield, carries a service, shares no types. |
| 2026-09-09 | Datastore never depends on starfield; starfield builds format checks from its own constants. |
| 2026-09-09 | `ContentCheck::magic(prefixes, trim)` and `custom(Arc<dyn Fn>)` replace `Magic(Vec<u8>)` / `Custom(fn -> bool)`; `NAIF/DAF` in DAF prefix lists; checks reject the wrong kind of thing, parsers validate format. |
| 2026-09-09 | Upstream credentials live only on the ephemeris server, the sole S3 writer. |
| 2026-09-09 | Access control is the tailnet boundary; no user auth in the datastore. |
| 2026-09-09 | Read path is presigned redirects; proxy mode is core (step 4), not optional. |
| 2026-09-09 | Off-tailnet: opt-in `STARFIELD_ALLOW_UPSTREAM=1`; no public-read bucket; not strict. |
| 2026-09-09 | Step 5 covers `Loader` and the datasources downloaders; live rot tests stay upstream-direct. |
| 2026-09-09 | `Immutable` only at first; `verify --repair` always explicit. |
