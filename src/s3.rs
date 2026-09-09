//! S3 transport for the server. All methods are synchronous: call them from a
//! blocking thread when embedding the store in an asynchronous application.
use crate::{check::digest_file, ArtifactKey, DatastoreError, Result};
use aws_sdk_s3::{
    config::Region,
    error::ProvideErrorMetadata,
    presigning::PresigningConfig,
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
    Client,
};
use aws_smithy_types::byte_stream::Length;
use std::{collections::HashMap, io::Write, path::Path, time::Duration};

#[cfg(not(test))]
const PART_BYTES: u64 = 64 * 1024 * 1024;
#[cfg(test)]
const PART_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct MirrorObject {
    pub sha256: String,
    pub bytes: u64,
    pub etag: Option<String>,
    pub source: Option<String>,
    pub provider_identity: Option<String>,
    pub fetched_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    Stored,
    AlreadyPresent,
}

pub struct S3Mirror {
    client: Client,
    bucket: String,
    prefix: String,
    runtime: Option<tokio::runtime::Runtime>,
}

impl S3Mirror {
    pub fn new(
        bucket: impl Into<String>,
        prefix: impl Into<String>,
        region: impl Into<String>,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let region = region.into();
        let config = runtime.block_on(
            aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(Region::new(region))
                .load(),
        );
        Self::with_client(Client::new(&config), runtime, bucket.into(), prefix.into())
    }

    fn with_client(
        client: Client,
        runtime: tokio::runtime::Runtime,
        bucket: String,
        prefix: String,
    ) -> Result<Self> {
        if bucket.is_empty() || bucket.contains('/') || bucket.chars().any(char::is_whitespace) {
            return Err(DatastoreError::Config("invalid S3 bucket name".into()));
        }
        let prefix = prefix.trim_matches('/').to_string();
        if !prefix.is_empty() {
            ArtifactKey::new(prefix.clone())?;
        }
        Ok(Self {
            client,
            bucket,
            prefix,
            runtime: Some(runtime),
        })
    }

    fn runtime(&self) -> &tokio::runtime::Runtime {
        self.runtime.as_ref().expect("runtime exists until drop")
    }

    pub fn head(&self, key: &ArtifactKey) -> Result<Option<MirrorObject>> {
        self.runtime().block_on(self.head_async(key))
    }

    /// Obtain a repair precondition even when application metadata is damaged.
    /// `None` means the object is absent; existing objects must have an ETag.
    pub fn observed_etag(&self, key: &ArtifactKey) -> Result<Option<String>> {
        self.runtime().block_on(async {
            match self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(key.mirror_path(&self.prefix))
                .send()
                .await
            {
                Ok(output) => output
                    .e_tag()
                    .map(|etag| Some(etag.to_owned()))
                    .ok_or_else(|| {
                        mirror_error("existing S3 object has no ETag for conditional repair")
                    }),
                Err(error)
                    if error
                        .raw_response()
                        .is_some_and(|r| r.status().as_u16() == 404) =>
                {
                    Ok(None)
                }
                Err(error) => Err(service_error(
                    "HEAD repair precondition",
                    error
                        .as_service_error()
                        .and_then(ProvideErrorMetadata::code),
                )),
            }
        })
    }

    async fn head_async(&self, key: &ArtifactKey) -> Result<Option<MirrorObject>> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key.mirror_path(&self.prefix))
            .send()
            .await
        {
            Ok(output) => {
                let metadata = output.metadata().cloned().unwrap_or_default();
                let bytes = output
                    .content_length()
                    .and_then(|n| u64::try_from(n).ok())
                    .ok_or_else(|| mirror_error("S3 object has no valid content length"))?;
                let sha256 = metadata
                    .get("sha256")
                    .cloned()
                    .ok_or_else(|| mirror_error("S3 object has no SHA-256 metadata"))?;
                validate_digest(&sha256)?;
                if metadata.get("bytes").and_then(|s| s.parse::<u64>().ok()) != Some(bytes) {
                    return Err(mirror_error(
                        "S3 object size metadata disagrees with content length",
                    ));
                }
                Ok(Some(MirrorObject {
                    sha256,
                    bytes,
                    etag: output.e_tag().map(str::to_owned),
                    source: metadata.get("source").cloned(),
                    provider_identity: metadata.get("provider-identity").cloned(),
                    fetched_at: metadata
                        .get("fetched-at")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0),
                }))
            }
            Err(error)
                if error
                    .raw_response()
                    .is_some_and(|r| r.status().as_u16() == 404) =>
            {
                Ok(None)
            }
            Err(error) => Err(service_error(
                "HEAD",
                error
                    .as_service_error()
                    .and_then(ProvideErrorMetadata::code),
            )),
        }
    }

    /// Streams bytes into a caller-owned temporary file. The caller validates
    /// them before publication, just as for HTTP mirrors.
    pub fn fetch(&self, key: &ArtifactKey, sink: &mut dyn Write) -> Result<bool> {
        self.runtime().block_on(async {
            let response = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key.mirror_path(&self.prefix))
                .send()
                .await;
            let mut output = match response {
                Ok(output) => output,
                Err(error)
                    if error
                        .raw_response()
                        .is_some_and(|r| r.status().as_u16() == 404) =>
                {
                    return Ok(false)
                }
                Err(error) => {
                    return Err(service_error(
                        "GET",
                        error
                            .as_service_error()
                            .and_then(ProvideErrorMetadata::code),
                    ))
                }
            };
            while let Some(chunk) = output
                .body
                .try_next()
                .await
                .map_err(|_| mirror_error("S3 body stream failed"))?
            {
                sink.write_all(&chunk)?;
            }
            Ok(true)
        })
    }

    pub fn presign_get(&self, key: &ArtifactKey, expiry: Duration) -> Result<url::Url> {
        if !(Duration::from_secs(300)..=Duration::from_secs(900)).contains(&expiry) {
            return Err(DatastoreError::Config(
                "presigned expiry must be between 5 and 15 minutes".into(),
            ));
        }
        self.runtime().block_on(async {
            let config = PresigningConfig::expires_in(expiry)
                .map_err(|_| mirror_error("invalid presigning expiry"))?;
            let request = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(key.mirror_path(&self.prefix))
                .presigned(config)
                .await
                .map_err(|_| mirror_error("S3 presigning failed"))?;
            url::Url::parse(request.uri()).map_err(|_| mirror_error("invalid presigned URL"))
        })
    }

    /// Conditionally publishes a validated local blob. Existing content is
    /// accepted only when its digest and length agree with this upload.
    pub fn put(&self, key: &ArtifactKey, path: &Path, meta: &MirrorObject) -> Result<PutOutcome> {
        self.write(key, path, meta, None)
    }

    /// Explicit repair, conditional on the ETag observed by the verifier.
    pub fn replace(
        &self,
        key: &ArtifactKey,
        path: &Path,
        meta: &MirrorObject,
        expected_etag: &str,
    ) -> Result<PutOutcome> {
        if expected_etag.is_empty() {
            return Err(mirror_error("repair requires an observed ETag"));
        }
        self.write(key, path, meta, Some(expected_etag))
    }

    fn write(
        &self,
        key: &ArtifactKey,
        path: &Path,
        meta: &MirrorObject,
        expected_etag: Option<&str>,
    ) -> Result<PutOutcome> {
        validate_digest(&meta.sha256)?;
        if path.metadata()?.len() != meta.bytes || digest_file(path)? != meta.sha256 {
            return Err(mirror_error(
                "local blob does not match upload digest and size",
            ));
        }
        let metadata = upload_metadata(meta)?;
        self.runtime().block_on(async {
            if expected_etag.is_none() {
                if let Some(existing) = self.head_async(key).await? {
                    return matching_existing(&existing, meta);
                }
            }
            // Multipart keeps memory bounded and supports artifacts larger than
            // S3's single-PUT limit. Completion carries the same precondition.
            if meta.bytes > PART_BYTES {
                return self
                    .multipart(key, path, meta, metadata, expected_etag)
                    .await;
            }
            for attempt in 0..3 {
                let body = ByteStream::from_path(path)
                    .await
                    .map_err(|_| mirror_error("cannot open upload body"))?;
                let mut request = self
                    .client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(key.mirror_path(&self.prefix))
                    .set_metadata(Some(metadata.clone()))
                    .body(body);
                request = if let Some(etag) = expected_etag {
                    request.if_match(etag)
                } else {
                    request.if_none_match("*")
                };
                match request.send().await {
                    Ok(_) => return Ok(PutOutcome::Stored),
                    Err(error) => {
                        let status = error.raw_response().map(|r| r.status().as_u16());
                        if status == Some(412) && expected_etag.is_none() {
                            return self.existing_after_race(key, meta).await;
                        }
                        if status == Some(409) && attempt < 2 {
                            continue;
                        }
                        return Err(service_error(
                            "PUT",
                            error
                                .as_service_error()
                                .and_then(ProvideErrorMetadata::code),
                        ));
                    }
                }
            }
            unreachable!()
        })
    }

    async fn existing_after_race(
        &self,
        key: &ArtifactKey,
        meta: &MirrorObject,
    ) -> Result<PutOutcome> {
        let existing = self.head_async(key).await?.ok_or_else(|| {
            mirror_error("conditional write raced with deletion; retry the operation")
        })?;
        matching_existing(&existing, meta)
    }

    async fn multipart(
        &self,
        key: &ArtifactKey,
        path: &Path,
        meta: &MirrorObject,
        metadata: HashMap<String, String>,
        expected_etag: Option<&str>,
    ) -> Result<PutOutcome> {
        let object_key = key.mirror_path(&self.prefix);
        let upload = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(&object_key)
            .set_metadata(Some(metadata))
            .send()
            .await
            .map_err(|e| {
                service_error(
                    "CreateMultipartUpload",
                    e.as_service_error().and_then(ProvideErrorMetadata::code),
                )
            })?;
        let id = upload
            .upload_id()
            .ok_or_else(|| mirror_error("S3 returned no upload id"))?;
        let result = async {
            let part_size = PART_BYTES.max(meta.bytes.div_ceil(10000));
            let mut parts = Vec::new();
            let mut offset = 0;
            while offset < meta.bytes {
                let length = part_size.min(meta.bytes - offset);
                let body = ByteStream::read_from()
                    .path(path)
                    .offset(offset)
                    .length(Length::Exact(length))
                    .build()
                    .await
                    .map_err(|_| mirror_error("cannot read multipart body"))?;
                let number = i32::try_from(parts.len() + 1)
                    .map_err(|_| mirror_error("too many upload parts"))?;
                let part = self
                    .client
                    .upload_part()
                    .bucket(&self.bucket)
                    .key(&object_key)
                    .upload_id(id)
                    .part_number(number)
                    .body(body)
                    .send()
                    .await
                    .map_err(|e| {
                        service_error(
                            "UploadPart",
                            e.as_service_error().and_then(ProvideErrorMetadata::code),
                        )
                    })?;
                let etag = part
                    .e_tag()
                    .ok_or_else(|| mirror_error("upload part has no ETag"))?;
                parts.push(
                    CompletedPart::builder()
                        .part_number(number)
                        .e_tag(etag)
                        .build(),
                );
                offset += length;
            }
            let mut request = self
                .client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(&object_key)
                .upload_id(id)
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(parts))
                        .build(),
                );
            request = if let Some(etag) = expected_etag {
                request.if_match(etag)
            } else {
                request.if_none_match("*")
            };
            match request.send().await {
                Ok(_) => Ok(PutOutcome::Stored),
                Err(error)
                    if error
                        .raw_response()
                        .is_some_and(|r| r.status().as_u16() == 412)
                        && expected_etag.is_none() =>
                {
                    self.existing_after_race(key, meta).await
                }
                Err(error) => Err(service_error(
                    "CompleteMultipartUpload",
                    error
                        .as_service_error()
                        .and_then(ProvideErrorMetadata::code),
                )),
            }
        }
        .await;
        if !matches!(result, Ok(PutOutcome::Stored)) {
            let _ = self
                .client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(&object_key)
                .upload_id(id)
                .send()
                .await;
        }
        result
    }
}

impl Drop for S3Mirror {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl crate::mirror::MirrorRead for S3Mirror {
    fn fetch(&self, key: &ArtifactKey, sink: &mut dyn Write) -> Result<bool> {
        self.fetch(key, sink)
    }
    fn location(&self, key: &ArtifactKey) -> String {
        format!("s3://{}/{}", self.bucket, key.mirror_path(&self.prefix))
    }
}

fn mirror_error(message: &str) -> DatastoreError {
    DatastoreError::Mirror(message.into())
}
fn service_error(operation: &str, code: Option<&str>) -> DatastoreError {
    // Service messages and request URLs can carry bearer capabilities.
    let code = code
        .filter(|s| s.len() < 80 && s.bytes().all(|b| b.is_ascii_alphanumeric()))
        .unwrap_or("request failed");
    DatastoreError::Mirror(format!("S3 {operation}: {code}"))
}
fn validate_digest(digest: &str) -> Result<()> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(mirror_error("invalid SHA-256 metadata"));
    }
    Ok(())
}
fn matching_existing(existing: &MirrorObject, proposed: &MirrorObject) -> Result<PutOutcome> {
    if existing.sha256 == proposed.sha256 && existing.bytes == proposed.bytes {
        Ok(PutOutcome::AlreadyPresent)
    } else {
        Err(mirror_error(
            "immutable S3 key already contains different content",
        ))
    }
}
fn upload_metadata(meta: &MirrorObject) -> Result<HashMap<String, String>> {
    let mut values = HashMap::from([
        ("sha256".into(), meta.sha256.clone()),
        ("bytes".into(), meta.bytes.to_string()),
        ("fetched-at".into(), meta.fetched_at.to_string()),
    ]);
    if let Some(source) = &meta.source {
        let mut url =
            url::Url::parse(source).map_err(|_| mirror_error("invalid source metadata"))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(mirror_error(
                "source metadata contains embedded credentials",
            ));
        }
        if matches!(url.scheme(), "http" | "https") {
            url.set_query(None);
            url.set_fragment(None);
            values.insert("source".into(), url.to_string());
        }
    }
    if let Some(identity) = &meta.provider_identity {
        values.insert("provider-identity".into(), identity.clone());
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read},
        net::TcpListener,
        sync::mpsc,
        thread,
    };

    struct Reply {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    }
    impl Reply {
        fn new(status: u16, body: &str) -> Self {
            Self {
                status,
                headers: vec![],
                body: body.into(),
            }
        }
        fn head(meta: &MirrorObject) -> Self {
            Self {
                status: 200,
                headers: vec![
                    ("Content-Length".into(), meta.bytes.to_string()),
                    ("x-amz-meta-sha256".into(), meta.sha256.clone()),
                    ("x-amz-meta-bytes".into(), meta.bytes.to_string()),
                    ("ETag".into(), "\"etag\"".into()),
                ],
                body: String::new(),
            }
        }
    }
    type Recorded = (String, HashMap<String, String>, u64);
    fn stub(replies: Vec<Reply>) -> (S3Mirror, mpsc::Receiver<Recorded>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            for reply in replies {
                let deadline = std::time::Instant::now() + Duration::from_secs(15);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(5))
                        }
                        Err(e) => panic!("S3 stub did not receive expected request: {e}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut headers = HashMap::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let (k, v) = line.split_once(':').unwrap();
                    headers.insert(k.to_ascii_lowercase(), v.trim().to_string());
                }
                let length = headers
                    .get("content-length")
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(0);
                let received =
                    std::io::copy(&mut reader.take(length), &mut std::io::sink()).unwrap();
                let _ = tx.send((first.clone(), headers, received));
                write!(
                    stream,
                    "HTTP/1.1 {} Stub\r\nConnection: close\r\n",
                    reply.status
                )
                .unwrap();
                if !reply
                    .headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                {
                    write!(stream, "Content-Length: {}\r\n", reply.body.len()).unwrap();
                }
                for (k, v) in reply.headers {
                    write!(stream, "{k}: {v}\r\n").unwrap();
                }
                write!(stream, "\r\n").unwrap();
                if !first.starts_with("HEAD ") {
                    stream.write_all(reply.body.as_bytes()).unwrap();
                }
            }
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let config = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .region(Region::new("us-east-1"))
            .credentials_provider(aws_credential_types::Credentials::new(
                "test-access",
                "test-secret",
                Some("test-session".into()),
                None,
                "test",
            ))
            .endpoint_url(endpoint)
            .force_path_style(true)
            .request_checksum_calculation(
                aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired,
            )
            .retry_config(aws_sdk_s3::config::retry::RetryConfig::standard().with_max_attempts(1))
            .build();
        let mirror = S3Mirror::with_client(
            Client::from_conf(config),
            runtime,
            "bucket".into(),
            "prefix".into(),
        )
        .unwrap();
        (mirror, rx, thread)
    }
    fn blob(bytes: &[u8]) -> (tempfile::NamedTempFile, MirrorObject) {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        let meta = MirrorObject {
            sha256: digest_file(file.path()).unwrap(),
            bytes: bytes.len() as u64,
            etag: None,
            source: Some("https://archive.test/data?token=secret".into()),
            provider_identity: Some("static:archive.test".into()),
            fetched_at: 1,
        };
        (file, meta)
    }
    #[test]
    fn conditional_put_streams_and_never_serializes_source_queries() {
        let (file, meta) = blob(b"data");
        let (mirror, rx, thread) = stub(vec![Reply::new(404, ""), Reply::new(200, "")]);
        assert_eq!(
            mirror
                .put(&ArtifactKey::new("data").unwrap(), file.path(), &meta)
                .unwrap(),
            PutOutcome::Stored
        );
        thread.join().unwrap();
        let requests = rx.into_iter().collect::<Vec<_>>();
        assert!(requests[0].0.starts_with("HEAD /bucket/prefix/data"));
        assert_eq!(requests[1].1["if-none-match"], "*");
        assert_eq!(
            requests[1].1["x-amz-meta-source"],
            "https://archive.test/data"
        );
        assert_eq!(requests[1].2, 4);
        assert_eq!(requests[1].1["x-amz-security-token"], "test-session");
    }
    #[test]
    fn race_accepts_matching_object_and_rejects_immutable_conflict() {
        let (file, meta) = blob(b"data");
        let (mirror, _, thread) = stub(vec![
            Reply::new(404, ""),
            Reply::new(412, "<Error><Code>PreconditionFailed</Code></Error>"),
            Reply::head(&meta),
        ]);
        assert_eq!(
            mirror
                .put(&ArtifactKey::new("data").unwrap(), file.path(), &meta)
                .unwrap(),
            PutOutcome::AlreadyPresent
        );
        thread.join().unwrap();
        let mut wrong = meta.clone();
        wrong.sha256 = "f".repeat(64);
        let (mirror, _, thread) = stub(vec![Reply::head(&wrong)]);
        assert!(mirror
            .put(&ArtifactKey::new("data").unwrap(), file.path(), &meta)
            .unwrap_err()
            .to_string()
            .contains("different content"));
        thread.join().unwrap();
    }
    #[test]
    fn get_and_presign_use_native_aws_credentials_without_exposing_runtime() {
        let (mirror, _, thread) = stub(vec![Reply::new(200, "bytes"), Reply::new(404, "")]);
        let key = ArtifactKey::new("data").unwrap();
        let mut bytes = Vec::new();
        assert!(mirror.fetch(&key, &mut bytes).unwrap());
        assert_eq!(bytes, b"bytes");
        assert!(!mirror.fetch(&key, &mut bytes).unwrap());
        let signed = mirror.presign_get(&key, Duration::from_secs(300)).unwrap();
        assert_eq!(signed.path(), "/bucket/prefix/data");
        assert!(signed.query_pairs().any(|(k, _)| k == "X-Amz-Signature"));
        assert!(signed
            .query_pairs()
            .any(|(k, v)| k == "X-Amz-Security-Token" && v == "test-session"));
        assert!(mirror.presign_get(&key, Duration::from_secs(1)).is_err());
        thread.join().unwrap();
    }

    #[test]
    fn synchronous_transport_works_from_server_blocking_pool() {
        let (mirror, _, thread) = stub(vec![Reply::new(404, "")]);
        let outer = tokio::runtime::Runtime::new().unwrap();
        outer.block_on(async move {
            let missing = tokio::task::spawn_blocking(move || {
                mirror.head(&ArtifactKey::new("missing").unwrap())
            })
            .await
            .unwrap()
            .unwrap();
            assert!(missing.is_none());
        });
        thread.join().unwrap();
    }

    #[test]
    fn explicit_repair_can_recover_missing_metadata_with_etag_precondition() {
        let (file, meta) = blob(b"data");
        let damaged = || Reply {
            status: 200,
            headers: vec![("ETag".into(), "\"old\"".into())],
            body: String::new(),
        };
        let (mirror, rx, thread) = stub(vec![damaged(), damaged(), Reply::new(200, "")]);
        let key = ArtifactKey::new("data").unwrap();
        assert!(mirror.head(&key).is_err());
        let etag = mirror.observed_etag(&key).unwrap().unwrap();
        assert_eq!(
            mirror.replace(&key, file.path(), &meta, &etag).unwrap(),
            PutOutcome::Stored
        );
        thread.join().unwrap();
        let requests = rx.into_iter().collect::<Vec<_>>();
        assert_eq!(requests[2].1["if-match"], "\"old\"");
        assert!(!requests[2].1.contains_key("if-none-match"));
    }
    #[test]
    fn multipart_upload_conditions_completion_and_aborts_failed_uploads() {
        let (mut file, _) = blob(b"");
        file.as_file_mut().set_len(PART_BYTES + 1).unwrap();
        let meta = MirrorObject {
            sha256: digest_file(file.path()).unwrap(),
            bytes: PART_BYTES + 1,
            etag: None,
            source: None,
            provider_identity: None,
            fetched_at: 1,
        };
        let mut part = Reply::new(200, "");
        part.headers.push(("ETag".into(), "\"part\"".into()));
        let (mirror, rx, thread) = stub(vec![Reply::new(404,""),Reply::new(200,"<InitiateMultipartUploadResult><UploadId>upload</UploadId></InitiateMultipartUploadResult>"),Reply {status:part.status,headers:part.headers.clone(),body:part.body.clone()},part,Reply::new(400,"<Error><Code>InvalidRequest</Code></Error>"),Reply::new(204,"")]);
        assert!(mirror
            .put(&ArtifactKey::new("big").unwrap(), file.path(), &meta)
            .is_err());
        thread.join().unwrap();
        let requests = rx.into_iter().collect::<Vec<_>>();
        assert_eq!(requests[2].2, PART_BYTES);
        assert_eq!(requests[3].2, 1);
        assert_eq!(requests[4].1["if-none-match"], "*");
        assert!(requests[5].0.starts_with("DELETE "));
    }
}
