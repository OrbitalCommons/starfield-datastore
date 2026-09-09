mod support;
use support::stub::{Response, Stub};

#[test]
fn local_stub_records_requests_and_serves_routes() {
    let stub = Stub::start("localhost");
    stub.route("/test", Response::ok(b"hello".to_vec()));
    let response = reqwest::blocking::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{}/test", stub.url()))
        .header("X-Test", "present")
        .send()
        .unwrap();
    assert_eq!(response.bytes().unwrap().as_ref(), b"hello");
    let requests = stub.requests();
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/test");
    assert_eq!(requests[0].headers["x-test"], "present");
}
