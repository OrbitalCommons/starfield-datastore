use starfield_datastore::{Artifact, ArtifactKey, Datastore, DatastoreError, Source};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

#[test]
fn html_prefix_aborts_without_waiting_for_the_advertised_body() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0];
        while !request.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 536870912\r\n\r\n")
            .unwrap();
        let mut prefix = [b' '; 8192];
        prefix[..6].copy_from_slice(b"<html>");
        socket.write_all(&prefix).unwrap();
        // Send no remainder. A client that waits for EOF or buffers the
        // complete body cannot reject until this socket times out.
        matches!(socket.read(&mut byte), Ok(0))
    });
    let cache = tempfile::TempDir::new().unwrap();
    let store = Datastore::builder()
        .cache_root(cache.path().to_path_buf())
        .allow_upstream(true)
        .progress(false)
        .build()
        .unwrap();
    let artifact = Artifact::new(
        ArtifactKey::new("stream/large").unwrap(),
        vec![Source::new(format!("http://{address}/large"))],
    );
    let error = store.get(&artifact).unwrap_err();
    assert!(server.join().unwrap(), "client must close after the prefix");
    assert!(matches!(error, DatastoreError::ContentRejected { .. }));
    assert!(!store.contains(&artifact.key));
    assert!(std::fs::read_dir(cache.path().join("tmp"))
        .unwrap()
        .next()
        .is_none());
}
