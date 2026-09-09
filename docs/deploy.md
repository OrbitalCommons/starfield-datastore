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
  carries artifact bytes.

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
| `NetrcProvider` | `~/.netrc`, mode `0600` | `machine urs.earthdata.nasa.gov login … password …`. A `default` entry is ignored on purpose. |
| `EnvProvider` | `STARFIELD_TOKEN_<HOST>` | Host uppercased, non-alphanumerics to `_`: `STARFIELD_TOKEN_ARCHIVE_STSCI_EDU`. Bearer tokens. |
| `OnePasswordProvider` | `STARFIELD_OP_ITEM=op://<vault>/<item>` | Feature `onepassword`; each host is a field of the item, read through `op` at request time. |

The chain is `Env → netrc → 1Password`, first match wins. The server's
`serve`, `mirror` and `verify --repair` all use it; nothing else in the
organisation needs an account on any archive.

## 4. Building

```
cargo build --release --features server
```

`server` implies `cli` and `mirror-s3`. Clients build with default features
only and never link the AWS SDK.

## 5. Running

Bind to the host's tailnet address; `serve` refuses a public address.

```
STARFIELD_CACHE_DIR=/var/lib/starfield-datastore/cache
STARFIELD_TOKEN_<HOST>=…                       # or ~/.netrc, or STARFIELD_OP_ITEM
AWS_REGION=<region>

starfield-datastore serve \
  --manifest /etc/starfield-datastore/ephemeris.toml \
  --bucket s3://<bucket>/<prefix> \
  --bind <tailnet-ip>:8080
```

A miss fetches upstream, validates, writes to S3, then redirects. A batch
pre-warm runs on a timer so the manifest is mirrored before anyone asks:

```
starfield-datastore mirror \
  --manifest /etc/starfield-datastore/ephemeris.toml \
  --to s3://<bucket>/<prefix>
```

`mirror` writes each artifact's `sha256` back into the manifest atomically,
under a `.lock` file beside it. Commit the pinned manifest.

### systemd

`/etc/systemd/system/starfield-datastore.service`:

```ini
[Unit]
Description=starfield ephemeris server
After=network-online.target tailscaled.service
Wants=network-online.target

[Service]
User=starfield
EnvironmentFile=/etc/starfield-datastore/env
ExecStart=/usr/local/bin/starfield-datastore serve \
  --manifest /etc/starfield-datastore/ephemeris.toml \
  --bucket s3://<bucket>/<prefix> --bind <tailnet-ip>:8080
Restart=on-failure
KillSignal=SIGTERM
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
```

`/etc/systemd/system/starfield-datastore-mirror.timer` runs the `mirror`
command nightly with the same environment file. The environment file is mode
`0600`, owned by `starfield`, and is the only place secrets exist on the box.

## 6. Clients

On the tailnet nothing but the mirror URL is needed:

```
STARFIELD_MIRROR=https://<tailnet-hostname>:8080
```

or in `~/.config/starfield/datastore.toml`:

```toml
mirror = "https://<tailnet-hostname>:8080"
```

Off the tailnet (an outside contributor, a GitHub-hosted runner):

```
STARFIELD_ALLOW_UPSTREAM=1
```

fetches from the archives with the caller's own credentials and populates the
local cache only. A CI job whose cache is already warm sets
`STARFIELD_OFFLINE=1` so it cannot reach out at all.

## 7. Operations

| Task | Command |
|---|---|
| Check the local cache | `starfield-datastore verify --manifest M` |
| Repair a corrupt local blob | `starfield-datastore verify --manifest M --repair` (removes the failed keys, refetches) |
| Check the mirror | `starfield-datastore verify --manifest M --at s3://<bucket>/<prefix>` (downloads and hashes every object; a HEAD is not a verification) |
| Repair the mirror | `… --at s3://<bucket>/<prefix> --repair` (conditional on the observed ETag) |
| Reclaim disk | `starfield-datastore gc --max-bytes N` (orphans and stale temp files first, then oldest keys; never runs implicitly) |
| See what is cached | `starfield-datastore list --bytes` |

`gc` is explicit because consumers hold paths, and sometimes memory maps,
into the cache. Do not schedule it on a host where a long-running consumer
may be mid-read.

## 8. What is deliberately not here

- No user database, tokens or TLS termination in the datastore. The tailnet
  is the boundary; put Tailscale Serve or a reverse proxy in front if TLS is
  wanted on the hop to the server.
- No raw-S3 read path from the tailnet (spec §2.5). Add it only if the server
  becomes a read bottleneck.
- No automatic eviction, no automatic repair, no resumable downloads
  (spec §16).
