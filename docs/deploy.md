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
sudo install -o starfield -g starfield -m 0600 deploy/env.example /etc/starfield-datastore/env
sudo install -m 0755 target/release/starfield-datastore /usr/local/bin/
sudo install -m 0644 deploy/starfield-datastore.service deploy/starfield-datastore-mirror.service deploy/starfield-datastore-mirror.timer /etc/systemd/system/
sudoedit /etc/starfield-datastore/env      # fill in bucket, bind address, region, tokens
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
