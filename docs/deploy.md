# Deploying the ephemeris server

The server is the only component that holds upstream credentials and the only
writer to the S3 mirror (spec §2). Everything below assumes one Linux host
joined to the organisation's tailnet. Values in angle brackets are
deployment-specific and live in the org's infrastructure notes, not here.

## 1. The bucket

- One private bucket, `<bucket>`, in `<region>`, with **Block Public Access**
  on. No bucket policy grants anonymous or cross-account read.
- Objects live under logical keys: `<prefix>/naif/spk/de440.bsp`, with the
  digest, size, source and provider identity in object metadata
  (`x-amz-meta-sha256`, `-bytes`, `-source`, `-provider-identity`,
  `-fetched-at`). Keys are immutable: every write is conditional
  (`If-None-Match: *`), and an explicit repair uses `If-Match` on the ETag
  the verifier observed.
- Presigned GET URLs expire after five minutes. Clients receive them via a
  302 from the server and download straight from S3; the server never
  proxies artifact bytes to a client. It does fetch and upload on a miss.

## 2. The writer role

The server runs under one IAM identity. Minimum policy, scoped to the prefix:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["s3:ListBucket"],
      "Resource": "arn:aws:s3:::<bucket>",
      "Condition": { "StringLike": { "s3:prefix": ["<prefix>/*"] } }
    },
    {
      "Effect": "Allow",
      "Action": [
        "s3:GetObject",
        "s3:PutObject",
        "s3:AbortMultipartUpload",
        "s3:ListMultipartUploadParts"
      ],
      "Resource": "arn:aws:s3:::<bucket>/<prefix>/*"
    }
  ]
}
```

`s3:DeleteObject` is deliberately absent. Nothing in the datastore deletes a
mirror object; a bad object is replaced through `verify --at … --repair`.

Credentials reach the server through the standard AWS chain (instance
profile, `AWS_PROFILE`, or environment). They are never expressed as a
datastore `Credential` and never touch the cache.

## 3. Upstream credentials

Only this host has them. Each provider is consulted lazily, per request,
keyed on the host actually being asked (Earthdata redirects through
`urs.earthdata.nasa.gov`, so that is the host to configure):

| Provider | Where | Notes |
|---|---|---|
| `NetrcProvider` | `~starfield/.netrc` (`/var/lib/starfield-datastore/.netrc`), mode `0600` | `machine urs.earthdata.nasa.gov login … password …`. A `default` entry is ignored on purpose. |
| `EnvProvider` | `STARFIELD_TOKEN_<HOST>` | Host uppercased, non-alphanumerics to `_`: `STARFIELD_TOKEN_ARCHIVE_STSCI_EDU`. Bearer tokens. |
| `OnePasswordProvider` | `STARFIELD_OP_ITEM=op://<vault>/<item>` | Feature `onepassword`; each host is a field of the item, read through `op` at request time. |

The chain is `Env → netrc → 1Password`, first match wins. The server's
`serve`, `mirror` and `verify --repair` all use it; nothing else in the
organisation needs an account on any archive.

Where secrets live on the box depends on which providers are in use: the
environment file for tokens and static AWS keys, `~starfield/.netrc` for
Basic credentials, the AWS profile or instance role for S3, and the `op`
session for 1Password. Keep it to as few of those as the archives allow;
none of them is ever copied into the cache, the manifest or S3 metadata.

## 4. Building

```
cargo build --release --features server
```

`server` implies `cli` and `mirror-s3`. Clients build with default features
only and never link the AWS SDK.

## 5. Setting up the host

```
sudo useradd --system --home /var/lib/starfield-datastore --shell /usr/sbin/nologin starfield
sudo install -d -o starfield -g starfield -m 0750 /var/lib/starfield-datastore /var/lib/starfield-datastore/cache
sudo install -d -o starfield -g starfield -m 0750 /etc/starfield-datastore
sudo install -o starfield -g starfield -m 0644 manifests/ephemeris.toml /etc/starfield-datastore/ephemeris.toml
sudo install -o starfield -g starfield -m 0600 deploy/config.toml /etc/starfield-datastore/config.toml
sudo install -o starfield -g starfield -m 0600 deploy/env.example /etc/starfield-datastore/env
sudo install -m 0755 target/release/starfield-datastore /usr/local/bin/
sudo install -m 0644 deploy/starfield-datastore.service deploy/starfield-datastore-mirror.service deploy/starfield-datastore-mirror.timer /etc/systemd/system/
sudoedit /etc/starfield-datastore/config.toml  # bucket, region, bind; set manifest to /etc/starfield-datastore/ephemeris.toml
sudoedit /etc/starfield-datastore/env          # optional environment-backed secrets
sudo systemctl daemon-reload
sudo systemctl enable --now starfield-datastore.service starfield-datastore-mirror.timer
```

`/etc/starfield-datastore` must be **writable by `starfield`**: `mirror`
writes each artifact's `sha256` back into the manifest atomically (temp file
and rename in the manifest's directory, under a `.lock` beside it). Copy the
pinned manifest back into the repository afterwards and commit it.

## 6. Running

Bind to the host's tailnet address; `serve` refuses a public address. By
hand, for a first smoke test:

```
export STARFIELD_CACHE_DIR=/var/lib/starfield-datastore/cache
export AWS_REGION=<region>
export STARFIELD_TOKEN_URS_EARTHDATA_NASA_GOV=<token>    # or ~/.netrc, or STARFIELD_OP_ITEM

starfield-datastore serve \
  --manifest /etc/starfield-datastore/ephemeris.toml \
  --bucket s3://<bucket>/<prefix> \
  --region "$AWS_REGION" \
  --bind <tailnet-ip>:8080
```

A miss fetches upstream, validates, writes to S3, then redirects. The batch
pre-warm mirrors the whole manifest before anyone asks:

```
starfield-datastore mirror \
  --manifest /etc/starfield-datastore/ephemeris.toml \
  --to s3://<bucket>/<prefix> \
  --region "$AWS_REGION"
```

The unit files in `deploy/` run both: `starfield-datastore.service` is the
server, `starfield-datastore-mirror.timer` fires the oneshot
`starfield-datastore-mirror.service` nightly, and both read
`/etc/starfield-datastore/config.toml` and an optional secret environment file
`/etc/starfield-datastore/env` (see `deploy/env.example`). The units use
`ProtectSystem=strict` with only the cache and config directories writable.

## 7. Clients

On the tailnet nothing but the mirror URL is needed. The server speaks plain
HTTP; the tailnet link is already encrypted end to end, and the artifact
bytes travel over HTTPS to S3 anyway. Use `https://` only if Tailscale Serve
or a reverse proxy terminates TLS in front of the server.

```
export STARFIELD_MIRROR=http://<tailnet-hostname>:8080
```

or in `~/.config/starfield/datastore.toml`:

```toml
mirror = "http://<tailnet-hostname>:8080"
```

Off the tailnet (an outside contributor, a GitHub-hosted runner):

```
export STARFIELD_ALLOW_UPSTREAM=1
```

fetches from the archives with the caller's own credentials and populates the
local cache only. A CI job whose cache is already warm sets
`STARFIELD_OFFLINE=1` so it cannot reach out at all.

## 8. Operations

| Task | Command |
|---|---|
| Check the local cache | `starfield-datastore verify --manifest M` |
| Repair a corrupt local blob | `starfield-datastore verify --manifest M --repair` (removes the failed keys, refetches) |
| Check the mirror | `starfield-datastore verify --manifest M --at s3://<bucket>/<prefix> --region <region>` (downloads and hashes every object; a HEAD is not a verification) |
| Repair the mirror | `… --at s3://<bucket>/<prefix> --repair` (conditional on the observed ETag) |
| Reclaim disk | `starfield-datastore gc --max-bytes N` (orphan blobs first, then oldest keys; never runs implicitly; never touches `tmp/`) |
| See what is cached | `starfield-datastore list --bytes` |
| Seed from a file already on disk | `starfield-datastore import --manifest M --key K --from PATH` (validated like a download; copies) |
| Drop one key | `starfield-datastore remove --key K` (how a corrupt blob shared with a key the manifest does not know gets cleared; `verify` names every key referencing it) |
| Crash leftovers | files in `<cache>/tmp/` older than any running transfer can be deleted by hand; `gc` never touches them |

`gc` is explicit because consumers hold paths, and sometimes memory maps,
into the cache. Do not schedule it on a host where a long-running consumer
may be mid-read.

## 9. What is deliberately not here

- No user database, tokens or TLS termination in the datastore. The tailnet
  is the boundary; put Tailscale Serve or a reverse proxy in front if TLS is
  wanted on the hop to the server.
- No raw-S3 read path from the tailnet (spec §2.5). Add it only if the server
  becomes a read bottleneck.
- No automatic eviction, no automatic repair, no resumable downloads
  (spec §16).

## 10. Container image and one-file configuration

Every push to `main` builds, tests, and publishes
`ghcr.io/orbitalcommons/starfield-datastore`. Tags are `latest`, `main`, and
`sha-<full-commit-sha>`. PRs build and smoke-test the image without publishing.
Publication waits for the Rust test suite and the container smoke test. The
image's source label links the package to this repository. GHCR initially
creates private packages; authenticated users with package access can pull
them. Package visibility can be changed in GitHub's package settings.

The image includes the server, CLI, and pinned ephemeris manifest. It runs as
UID/GID `10001:10001`, supports a read-only root filesystem, and stores all
cache and verification scratch files in the mounted cache volume. It includes
the env/basic/netrc credential sources and native AWS credentials; 1Password
requires a custom image built with that feature and an installed `op` CLI.

Copy `deploy/config.toml`, edit its bucket/region/bind settings, then run on a
Linux host joined to the tailnet:

```sh
docker volume create starfield-cache
docker run -d --name starfield-datastore --restart unless-stopped \
  --network host --read-only \
  --mount type=volume,src=starfield-cache,dst=/var/lib/starfield-datastore/cache \
  --mount type=bind,src="$(pwd)/config.toml",dst=/etc/starfield-datastore/config.toml,readonly \
  ghcr.io/orbitalcommons/starfield-datastore:latest
```

The default command selects `services.ephemeris` from that file. Use a tailnet
bind address to serve remote clients; loopback is the safe example default.
Host networking is intentional: the server refuses public binds, and binding
to a container's loopback with ordinary `-p` forwarding is not sufficient.
The host and container must make the config readable by UID 10001. Mount
`/var/lib/starfield-datastore/.netrc` read-only with mode 0600 and that owner,
or pass named secret variables with Docker `--env-file`. AWS uses its native
chain: an instance role, mounted AWS profile, or AWS environment credentials.
Never pass credentials as Docker build arguments.

```sh
# Use another service profile from the same file:
docker run --rm --network host \
  --mount type=volume,src=starfield-cache,dst=/var/lib/starfield-datastore/cache \
  --mount type=bind,src="$(pwd)/config.toml",dst=/etc/starfield-datastore/config.toml,readonly \
  ghcr.io/orbitalcommons/starfield-datastore:latest \
  --config /etc/starfield-datastore/config.toml --service catalogs serve

# Build the same image locally:
docker build -t starfield-datastore:local .
```

For `mirror`, the manifest directory must be mounted writable: that command
locks and atomically updates its pins. Point the service's `manifest` at the
mounted copy, rather than the bundled `/usr/share` copy.

The shared config has top-level cache settings, a `[[credentials]]` list, and
`[services.<name>]` profiles. Credential types are `env` (`variable`), `basic`
(`username`, `password_env`), `netrc`, and `onepassword` (`reference`). A host
list explicitly authorizes that credential for each exact host; no wildcard
or suffix matching occurs. Secrets are read only when that host is requested.
1Password is invoked per request, so rotation is picked up without restart.

A profile's `credentials = [names]` is an explicit selection with no ambient
fallback; `[]` disables upstream credentials. Omit it to inherit the global
configured/default chain. Different profiles can select different credentials
for the same host; two recognized entries cannot claim one host in a single
active selection. Unknown types are retained as `Unrecognized(serde_json::Value)`,
skipped, and reported by name/type without logging their payload. Unknown
payloads support JSON-compatible TOML values; datetime literals must be quoted
as strings. Malformed known types and unknown credential references are errors.

When profiles use different credentials for the same host, select one with
`--service` or `DatastoreConfig::builder_for_service`. Plain `from_env()` uses
the global selection and refuses ambiguous hosts rather than choosing an
account implicitly.

CLI flags override profile values. For region, flags override AWS environment
variables, which override the profile. Existing cache-setting environment
overrides still apply. Relative manifest/cache paths resolve beside the config
file. Select profiles with `--service`; a sole profile is selected automatically.
