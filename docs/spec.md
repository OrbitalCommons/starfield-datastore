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
/// ASCII alphanumerics and `-._/` only; no empty, `.` or `..` component;
/// at most 1024 bytes. The key is also the index path and the mirror
/// object path, so it must be safe as both.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArtifactKey(String);

impl ArtifactKey {
    pub fn new(key: impl Into<String>) -> Result<Self>;   // validates shape
    pub fn as_str(&self) -> &str;
    /// Object path under a mirror prefix.
    pub fn mirror_path(&self, prefix: &str) -> String;
}

/// Where to obtain an artifact upstream.
/// `Debug` prints the URL redacted: a source may be a signed URL whose
/// query string is a bearer capability. Userinfo in a URL is rejected
/// outright; credentials come from a `CredentialProvider`.
#[derive(Clone)]
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
    /// May be empty: an artifact that must be obtained by hand (an
    /// ECOSTRESS granule ordered from a portal) is `import`ed, and a miss
    /// on it fails with `ManualRequired` carrying `provenance.description`.
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
    /// Streaming: built-in checks read only what they need; `Custom` alone
    /// loads the whole file.
    pub fn check_file(&self, path: &Path) -> std::result::Result<(), CheckFailure>;
    /// Decide what can be decided from the leading bytes (`NotHtml`,
    /// `Magic`), so a login page is rejected before 12 GB of it arrive. A
    /// head too short to decide passes; `check_file` is authoritative.
    pub fn check_prefix(&self, head: &[u8]) -> std::result::Result<(), CheckFailure>;
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

The store shows the first 8 KB of every transfer to `check_prefix` before
letting the rest of the body in, then runs `check_file` on the completed
temp file. `Sha256` nodes are compared against the digest computed while
streaming rather than rehashed; every `Sha256` in the tree must match, so
two conflicting pins can never pass.

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
#[derive(Clone)]
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
}

pub trait CredentialProvider: Send + Sync {
    /// Credential for the host of the request actually being made, or None.
    fn credential_for(&self, host: &str) -> Result<Option<Credential>>;
    /// Identity used, for the manifest. Never the secret.
    fn identity_for(&self, host: &str) -> Option<String>;
}

pub struct NetrcProvider;          // ~/.netrc (mode 0600; `default` ignored)
pub struct EnvProvider;            // STARFIELD_TOKEN_<HOST>, bearer
pub struct OnePasswordProvider { pub item: String }   // `op read op://…/<host>`, never to disk
pub struct StaticProvider(pub HashMap<String, Credential>);  // tests
pub struct ChainProvider(pub Vec<Box<dyn CredentialProvider>>);
```

`EnvProvider` maps a host to a variable by uppercasing it and replacing every
non-alphanumeric with `_`: `urs.earthdata.nasa.gov` reads
`STARFIELD_TOKEN_URS_EARTHDATA_NASA_GOV`. `NetrcProvider` refuses a
world-readable file and ignores a `default` entry, so a credential can never
be sent to a host nobody named.

S3 is not a `Credential`. The mirror's writer identity comes from the
standard AWS chain inside `S3Mirror` and is never modelled here; there is no
`AwsSigV4` variant and no `AwsProvider`.

Where they run: the provider chain is **server-side configuration**. Clients
need no provider at all on the tailnet. Under `STARFIELD_ALLOW_UPSTREAM` a
client fetches with its own credentials: `DatastoreBuilder::from_env`
installs `Chain(Env, Netrc[, OnePassword])` when nothing explicit was set,
every provider looks up lazily per request, and a plain `Datastore::builder()`
installs nothing.

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
6. Redirects are followed by hand (never by the HTTP client) so rules 2 and
   4 apply per hop: each hop uses the credential held for *its own* host;
   with `trust_redirects` the original source's credential is additionally
   forwarded to a hop whose host has none. Every hop's URL is re-validated —
   no userinfo, `http(s)` only, no `https` → `http` downgrade — and at most
   ten hops are followed.
7. The mirror client runs the same engine with **no provider and no cookie
   jar**, so nothing the ephemeris server sets can ride along on the
   presigned redirect it issues. The upstream engine keeps a per-host cookie
   jar: Earthdata authorises the final hop with a cookie set two redirects
   earlier.

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
    /// Seed the cache from a file already on disk — a legacy flat cache, a
    /// granule obtained by hand — validated exactly as a download would be.
    /// Copies; never moves.
    pub fn import(&self, artifact: &Artifact, path: &Path) -> Result<PathBuf>;

    pub fn contains(&self, key: &ArtifactKey) -> bool;
    /// The index sidecar, if cached.
    pub fn entry(&self, key: &ArtifactKey) -> Option<IndexEntry>;
    pub fn remove(&self, key: &ArtifactKey) -> Result<()>;
    pub fn keys(&self) -> Result<Vec<ArtifactKey>>;
    /// Every blob on disk, orphans included; shared blobs count once.
    pub fn total_bytes(&self) -> Result<u64>;
    /// Rehash every blob; report keys whose content no longer matches.
    pub fn verify(&self) -> Result<Vec<VerifyFailure>>;
    /// Reclaim orphan blobs, then evict least-recently-fetched keys until
    /// the store is within the budget. Never runs implicitly.
    pub fn gc(&self, max_bytes: u64) -> Result<Vec<ArtifactKey>>;
    pub fn cache_root(&self) -> &Path;
    pub fn max_bytes(&self) -> Option<u64>;
}

impl DatastoreBuilder {
    /// Pre-filled from env and the config file; later calls override.
    pub fn from_env() -> Result<Self>;
    pub fn cache_root(self, path: PathBuf) -> Self;
    pub fn mirror(self, mirror: Mirror) -> Self;
    /// Drop a mirror picked up from the environment. The server uses this
    /// so it never resolves through itself.
    pub fn without_mirror(self) -> Self;
    pub fn credentials(self, provider: Box<dyn CredentialProvider>) -> Self;
    /// Permit the upstream layer. Default false; see §2.4.
    pub fn allow_upstream(self, allow: bool) -> Self;
    /// Disable layers 2 and 3. What CI sets once the mirror is warm.
    pub fn offline(self, offline: bool) -> Self;
    /// Connect timeout only. There is no read or total timeout: a 12 GB
    /// mosaic takes as long as it takes.
    pub fn timeout(self, timeout: Duration) -> Self;
    pub fn progress(self, enabled: bool) -> Self;
    /// `(received, total)` per chunk, instead of bars.
    pub fn on_progress(self, callback: Box<dyn Fn(u64, Option<u64>) + Send + Sync>) -> Self;
    /// Advisory budget; applied only by an explicit `gc`.
    pub fn max_bytes(self, max: u64) -> Self;
    pub fn build(self) -> Result<Datastore>;
}

/// The index sidecar (§12). Records a provider *identity*, never a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub digest: String,            // lowercase hex SHA-256; the blob's address
    pub bytes: u64,
    pub fetched_at: u64,           // Unix seconds
    pub source: Option<String>,    // sanitised origin+path, mirror location, or "local:import"
    pub etag: Option<String>,
    pub provider_identity: Option<String>,
    pub layer: Layer,
}

/// `verify` reports exactly what `get` would refuse: a missing blob, a
/// digest mismatch, or a size that disagrees with the sidecar.
pub struct VerifyFailure {
    pub key: ArtifactKey,
    pub expected: String,        // digest the index records
    pub actual: Option<String>,  // digest the blob hashes to; None if missing
    pub reason: String,
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
`MirrorUnreachable`, which names `STARFIELD_ALLOW_UPSTREAM` and carries the
reason (connection refused, the server's own error text, "no mirror
configured", or "has no entry").

Invariants the store holds:

- **The chain never writes a mirror.** `Mirror::*::writable` is
  informational; a client's upstream fill populates local disk only. The
  ephemeris server uploads explicitly after its own `get`.
- **A hit is re-verified.** `get` on a cached key checks the file length
  against the sidecar, rehashes the blob, and re-runs the artifact's check.
  A mismatch is `ContentRejected` naming the corruption and stays an error
  until `verify` and an explicit `remove` or repair; nothing is silently
  refetched. `peek` and `entry` never rehash. (Rehashing every hit costs a
  read of the file; an mtime/verified-handle shortcut is a later decision.)
- **A moved pin is a miss.** If the artifact's `Sha256` no longer matches
  the cached digest the store fetches afresh; the old blob becomes an orphan
  reclaimed by the next `gc`, so a path handed out earlier stays valid.
- **Publication fails closed.** A blob already at the target address that no
  longer hashes to it is never overwritten by an ordinary download or
  import; the caller gets `ContentRejected` and `verify` names every key
  that references it.
- **One fetch per key per host.** A per-key advisory lock serialises
  concurrent `get`s across processes; the losers find a hit. A store-wide
  lock, always taken after the key lock, makes blob publication atomic with
  respect to `remove`/`gc` reference scans.
- **`gc` is the only eviction.** `STARFIELD_CACHE_MAX` is a default for the
  CLI's `gc`, not a trigger. Orphans go first; then keys, oldest
  `fetched_at` first. `tmp/` is never touched: a temp file's age proves
  nothing about whether its transfer is still running.
- **Single-source errors are verbatim.** With one source,
  `ContentRejected`, `NoCredential` and `CredentialRejected` surface as
  themselves; everything else, and every multi-source failure, aggregates
  into `AllSourcesFailed`.

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
    #[error("{key} is not cached and the mirror is unreachable ({reason}); set STARFIELD_ALLOW_UPSTREAM=1 to fetch from the archive")]
    MirrorUnreachable { key: ArtifactKey, reason: String },
    #[error("{key} is not cached and has no sources; obtain it manually: {instructions}")]
    ManualRequired { key: ArtifactKey, instructions: String },
    #[error("all {} sources failed for {key}: {}", .attempts.len(), .attempts.join("; "))]
    AllSourcesFailed { key: ArtifactKey, attempts: Vec<String> },
    #[error("invalid artifact key: {0}")]
    InvalidKey(String),
    #[error("mirror error: {0}")]
    Mirror(String),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("manifest error: {0}")]
    Manifest(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The URL is stripped from the wrapped error: a presigned URL is a
    /// bearer capability and must not end up in a log line.
    #[error("HTTP request failed")]
    Http(#[source] reqwest::Error),
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

Manifest shapes, all round-tripped and fail-closed on unknown fields:

| Field | Forms |
|---|---|
| `sources` | `["https://…"]` or `[{ url = "https://…", trust_redirects = true }]`; may be empty; userinfo and non-`http(s)` rejected |
| `sha256` | top level, optional; parsed as `All([Sha256, check])` and written back the same way |
| `check` | `{ none = true }` · `{ not_html = true }` · `{ min_bytes = N }` · `{ sha256 = "…" }` · `{ magic = ["DAF/SPK", [255, 0]], trim_leading_whitespace = false }` · `{ all = […] }`; `"none"` / `"not-html"` accepted on input; default `default_binary()` |
| `bytes` | `expected_bytes`; a mismatch is a rejection like any other pin |
| `freshness` | `"immutable"` only |

`Manifest::pin_sha256(key, digest)` is idempotent and refuses to replace a
different existing pin; `merge` replaces matching keys in place and appends
new ones. Errors name the key or the TOML line and column, never a source
line (a signed URL in a manifest is already a mistake, and echoing it would
be a second one).

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

Two more commands exist because the boundary needs an escape hatch:
`import --manifest M --key K --from PATH` seeds the cache from a file on
disk, validated like a download; `remove --key K` drops one key, which is
how an operator clears a corrupt blob that `verify` reports under an alias
the manifest does not know.

### 10.1 The server

`serve --manifest M --bucket s3://bucket/prefix --bind ADDR`. `ADDR` must be
loopback or a Tailscale address (`100.64/10`, `fd7a:115c:a1e0::/48`); a
public bind is refused. The store it resolves with is built from the
environment with the mirror removed (`without_mirror`) and upstream allowed,
so the server never resolves through itself.

| Request | Response |
|---|---|
| `GET /artifact/<key>`, key in manifest, object in S3 | `302 Location: <presigned GET, 5 min>`, `Cache-Control: no-store`, `X-Artifact-Sha256`, `X-Artifact-Bytes` |
| … object not in S3 | `Datastore::get` on a blocking thread, then an explicit `S3Mirror::put`, then as above |
| … S3 metadata disagrees with the manifest's pins or size | `422` naming `verify --at … --repair` |
| … upstream or credential failure | `502` with the `DatastoreError` text |
| key not in manifest, or malformed | `404` |
| `GET /healthz` | `200` |

Work is bounded by a semaphore of eight blocking fills; the runtime is the
server's own, and `Datastore` is only ever called through `spawn_blocking`
(`Runtime::block_on` inside an async context panics). Shutdown is graceful
on `SIGINT` and `SIGTERM`. Artifacts without a `license` are refused: the
server is redistributing.

### 10.2 The S3 layout

Objects live under logical keys — `<prefix>/<key>`, exactly
`ArtifactKey::mirror_path` — not under content addresses. The digest,
size, sanitised source, provider identity and fetch time are object
metadata (`x-amz-meta-sha256`, `-bytes`, `-source`, `-provider-identity`,
`-fetched-at`). A presign is therefore a function of the key alone, the
bucket is browsable, and `verify --at` can read metadata without a
download. It still downloads: **a HEAD is not a verification**; `verify
--at` hashes every body.

Writes are conditional (`If-None-Match: *`); a `412` after a race is
resolved by a HEAD and accepted only if the existing digest and size match.
Objects over 64 MiB go up multipart with a CRC32 composite checksum and
per-part checksums on completion, which carries the same precondition; a
failed upload is aborted. Repair is `replace(key, path, meta, etag)` with
`If-Match` on the ETag the verifier observed. `S3Mirror` owns a private
tokio runtime; its API is synchronous and leaks no tokio types.

## 11. Configuration

Resolved from: explicit builder call, then env, then
`~/.config/starfield/datastore.toml`, then defaults.

| Setting | Env | File key | Default |
|---|---|---|---|
| Cache root | `STARFIELD_CACHE_DIR` | `cache_dir` | `$XDG_CACHE_HOME/starfield` or `~/.cache/starfield` |
| Mirror | `STARFIELD_MIRROR` (`https://…` or `s3://bucket/prefix`) | `mirror` | none (the org config sets the ephemeris server URL) |
| Mirror region | `STARFIELD_MIRROR_REGION`, else `AWS_REGION` | `mirror_region` | required for `s3://` |
| Allow upstream | `STARFIELD_ALLOW_UPSTREAM` | `allow_upstream` | false |
| Offline | `STARFIELD_OFFLINE` | `offline` | false |
| Max cache bytes | `STARFIELD_CACHE_MAX` | `cache_max` | unbounded; used only by `gc` |
| Config file | `STARFIELD_DATASTORE_CONFIG` | — | `$XDG_CONFIG_HOME/starfield/datastore.toml` |
| 1Password item | `STARFIELD_OP_ITEM` | — | none (feature `onepassword`) |

Booleans accept `1/0`, `true/false`, `yes/no`, `on/off`; anything else is a
`Config` error, as is an unknown key in the file. Precedence is realised by
`DatastoreBuilder::from_env()`: it fills every field the caller has not set
from env, then file; calls made on the returned builder override both.
`Datastore::builder()` reads nothing and is what tests use.

The cache root stays `~/.cache/starfield`, shared with the existing
`data::downloader`, so nobody re-downloads 114 MB on the day of the switch.

## 12. Storage layout

```
~/.cache/starfield/
  blobs/<aa>/<sha256>          # content-addressed, immutable once written
  index/<key path>.json        # { digest, bytes, fetched_at, source, etag, provider_identity, layer }
  tmp/                         # in-flight; temp + fsync + rename, then the directory is fsynced
  locks/<sha256(key)>.lock     # advisory, so two processes do not both pull 12 GB
  locks/.store.lock            # store-wide; publication vs. reference scans, taken after a key lock
```

A sidecar whose digest is not 64 lowercase hex characters is a corrupt
sidecar, reported as an I/O error, never used as a path. `fetched_at` is
Unix seconds. `source` is the sanitised origin and path (never a query
string), the mirror location (`https://…/artifact/<key>` or
`s3://bucket/prefix/<key>`), or `local:import`.

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
| `cli` | no | `clap`; the `starfield-datastore` binary without S3 |
| `server` | no | `cli` + `mirror-s3` + `axum`/`tokio`; adds `serve` |

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
- **The upstream layer is a first-class, tested path**, not an escape hatch:
  the default suite exercises it against a local HTTP stub (redirect handling,
  validation, local-only fill under `STARFIELD_ALLOW_UPSTREAM`). focalplane's
  CI runs on plain GitHub-hosted runners with no tailnet and resolves every
  artifact this way, including large kernels and mosaic tiers.
- **`verify --at s3://…` downloads and hashes every object.** Metadata is
  what the writer said; only the body is what the reader gets.
- **A cache hit and an upstream fetch are byte-identical by construction**, and
  a test says so: blobs are stored under their SHA-256, a manifest `sha256` is
  verified on every layer, and `verify` rehashes the local store. Consumers
  that pin by content (focalplane, the datasource crates) rely on this.

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
| 2026-09-09 | Implementation split across two sessions: core (primitives, providers, manifests, S3, CLI, server, CI) and store (fetch engine, local layer, resolution chain, config, spec). Every PR cross-reviewed before merge; CI green is a hard gate. |
| 2026-09-09 | Sync public API. `S3Mirror` owns a private tokio runtime; the server calls `Datastore` only through `spawn_blocking`. |
| 2026-09-09 | Redirects followed by hand, per-hop credential lookup, every hop re-validated, no `https`→`http` downgrade. Mirror engine has no provider and no cookie jar; upstream engine keeps a cookie jar for Earthdata. |
| 2026-09-09 | `Credential::AwsSigV4` and `AwsProvider` removed: S3 uses the native AWS chain inside `S3Mirror`. |
| 2026-09-09 | The chain never writes a mirror; the server uploads explicitly after `get`. `writable` is informational. |
| 2026-09-09 | S3 layout is logical keys with digest/size/source metadata, conditional puts, multipart with CRC32 composite checksums, `If-Match` repair. HEAD is not a verification. |
| 2026-09-09 | A cache hit is rehashed and re-validated on every `get` for the first release; corruption is an error until explicit repair; a moved pin is a miss. Optimisation deferred. |
| 2026-09-09 | Publication fails closed on a corrupt blob at the same address; no self-heal. |
| 2026-09-09 | `gc` is explicit only, reclaims orphans then oldest keys, never touches `tmp/`. `STARFIELD_CACHE_MAX` is a default for the CLI, not a trigger. Leases for held paths deferred. |
| 2026-09-09 | `import` added for validated local seeding (legacy caches, manually obtained granules); `ManualRequired` carries `provenance.description`. |
| 2026-09-09 | Early rejection: the first 8 KB of every transfer go through `check_prefix` before the rest is accepted. |
| 2026-09-09 | `from_env` installs the caller's own credential chain (env, netrc, optional 1Password via `STARFIELD_OP_ITEM`); `builder()` installs nothing. |
| 2026-09-09 | `ContentCheck::custom` is code-only; manifests express the declarative set. `{ none = true }` is the spelled opt-out. |
| 2026-09-09 | Timeouts: connect only. No read or total timeout. |
| 2026-09-09 | Deferred, recorded: resumable `Range` downloads; leases protecting held paths from `gc`; hit-verification shortcuts. Not chosen: weakening TLS for any archive (the NSA archive's broken chain stays a documented escape hatch in `starfield-datasources`). |
