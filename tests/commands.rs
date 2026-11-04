// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Stub {
    origin: String,
    server: Option<thread::JoinHandle<Vec<Vec<u8>>>>,
    stopped: Arc<AtomicBool>,
}
impl Stub {
    fn new(responses: Vec<String>) -> Self {
        Self::bytes(responses.into_iter().map(String::into_bytes).collect())
    }
    fn bytes(responses: Vec<Vec<u8>>) -> Self {
        Self::with_raw_hook(responses, |_| {})
    }
    fn with_hook(responses: Vec<Vec<u8>>, mut on_request: impl FnMut(&str) + Send + 'static) -> Self {
        Self::with_raw_hook(responses, move |raw| {
            on_request(std::str::from_utf8(raw).unwrap())
        })
    }
    fn with_raw_hook(responses: Vec<Vec<u8>>, on_request: impl FnMut(&[u8]) + Send + 'static) -> Self {
        Self::serve(responses, on_request, Duration::ZERO)
    }
    fn serve(
        responses: Vec<Vec<u8>>,
        mut on_request: impl FnMut(&[u8]) + Send + 'static,
        quiet_period: Duration,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let server_stopped = Arc::clone(&stopped);
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let deadline = Instant::now() + Duration::from_secs(15);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                !server_stopped.load(Ordering::Acquire) && Instant::now() < deadline,
                                "expected request did not arrive"
                            );
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = match stream.read(&mut buf) {
                        Ok(n) => n,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => panic!("{error}"),
                    };
                    assert!(n > 0);
                    raw.extend_from_slice(&buf[..n]);
                    if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&raw[..end]);
                        let len = headers
                            .lines()
                            .find_map(|line| {
                                line.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|x| x.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if raw.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                on_request(&raw);
                requests.push(raw);
                stream.write_all(&response).unwrap();
            }
            let deadline = Instant::now() + quiet_period;
            loop {
                match listener.accept() {
                    Ok(_) => panic!("unexpected retry after final response"),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if server_stopped.load(Ordering::Acquire) && Instant::now() >= deadline {
                            break;
                        }
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => panic!("{e}"),
                }
            }
            requests
        });
        Self {
            origin,
            server: Some(server),
            stopped,
        }
    }
    fn finish(self) -> Vec<String> {
        self.finish_bytes()
            .into_iter()
            .map(|raw| String::from_utf8(raw).unwrap())
            .collect()
    }
    fn finish_bytes(mut self) -> Vec<Vec<u8>> {
        self.stop_and_join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    }
    fn stop_and_join(&mut self) -> thread::Result<Vec<Vec<u8>>> {
        self.stopped.store(true, Ordering::Release);
        self.server.take().unwrap().join()
    }
}
impl Drop for Stub {
    fn drop(&mut self) {
        if self.server.is_some() {
            let result = self.stop_and_join();
            // Always join the worker, but preserve the original failure during unwinding.
            if !thread::panicking()
                && let Err(panic) = result
            {
                std::panic::resume_unwind(panic);
            }
        }
    }
}
fn response(status: &str, body: Value, headers: &str) -> String {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}",
        body.len()
    )
}
fn page(data: Value, cursor: Option<&str>) -> Value {
    json!({"object_type":"LIST","data":data,"has_more":cursor.is_some(),"next_cursor":cursor})
}
fn command(origin: &str, args: &[&str], stdin: Option<&str>) -> Output {
    let dir = std::env::temp_dir().join(format!(
        "lingohub-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_lingohub"));
    command
        .env_clear()
        .current_dir(&dir)
        .env("HOME", &dir)
        .env("LINGOHUB_API_KEY", "test-secret-do-not-print")
        .args(["--base-url", origin])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !args.contains(&"--color") {
        command.args(["--color", "never"]);
    }
    // Windows needs SystemRoot to locate its networking providers even in an isolated test process.
    #[cfg(windows)]
    command.env(
        "SystemRoot",
        std::env::var_os("SystemRoot").expect("Windows SystemRoot"),
    );
    let mut child = command.spawn().unwrap();
    if let Some(input) = stdin {
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    }
    drop(child.stdin.take());
    let result = child.wait_with_output().unwrap();
    let _ = std::fs::remove_dir_all(dir);
    assert!(!String::from_utf8_lossy(&result.stdout).contains("test-secret-do-not-print"));
    assert!(!String::from_utf8_lossy(&result.stderr).contains("test-secret-do-not-print"));
    result
}
fn assert_error_contains(output: &Output, expected: &str) {
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains(expected),
        "expected {expected:?}, received {error}"
    );
}

fn success(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_download_metadata_error(out: &Output, expected: &str) {
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stderr).unwrap(),
        json!({"error": expected})
    );
}

fn header_values<'a>(raw: &'a str, name: &str) -> Vec<&'a str> {
    raw.split("\r\n\r\n")
        .next()
        .unwrap()
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
        .collect()
}

#[test]
fn stub_drop_rejects_unexpected_requests_without_explicit_finish() {
    let stub = Stub::new(vec![]);
    let _connection = std::net::TcpStream::connect(stub.origin.strip_prefix("http://").unwrap()).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(stub)));
    let panic = result.expect_err("dropping a stub must reject an unexpected request");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"unexpected retry after final response")
    );
}

#[test]
fn stub_drop_rejects_missing_requests_without_explicit_finish() {
    let stub = Stub::new(vec![response("200 OK", json!({}), "")]);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(stub)));
    let panic = result.expect_err("dropping a stub must reject an unconsumed response");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"expected request did not arrive")
    );
}

#[test]
fn stub_drop_preserves_an_existing_panic() {
    let result = std::panic::catch_unwind(|| {
        let _stub = Stub::new(vec![response("200 OK", json!({}), "")]);
        panic!("original test failure");
    });
    assert_eq!(
        result.unwrap_err().downcast_ref::<&str>(),
        Some(&"original test failure")
    );
}

#[test]
fn stub_drop_accepts_satisfied_expectations_without_explicit_finish() {
    let body = json!({"object_type":"WORKSPACE","id":"space_abc","name":"Example"});
    let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
    let out = command(&stub.origin, &["--json", "workspace"], None);
    assert_eq!(success(&out), body);
}

#[test]
fn workspace_preserves_wire_values_and_reports_headers_separately() {
    let body = json!({"object_type":"WORKSPACE","id":"space_abc","name":"Example","created_at":"2026-09-29T00:00:00.123Z","extra":null});
    let stub = Stub::new(vec![response(
        "200 OK",
        body.clone(),
        "ETag: \"version\"\r\nX-Request-Id: req-1\r\n",
    )]);
    let out = command(&stub.origin, &["--json", "--include-headers", "workspace"], None);
    assert_eq!(success(&out), body);
    assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"version\""));
    let requests = stub.finish();
    assert!(requests[0].starts_with("GET /v2/workspace "));
    assert!(
        requests[0]
            .to_lowercase()
            .contains("authorization: bearer test-secret-do-not-print")
    );
}
#[test]
fn explicit_opaque_cursors_preserve_leading_hyphens() {
    for resource in ["stack", "project", "style-guide"] {
        for cursor in ["-cursor", "--opaque+/=", "--all"] {
            let body = page(json!([]), None);
            let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
            let out = command(
                &stub.origin,
                &["--json", resource, "list", "--cursor", cursor, "--limit", "1"],
                None,
            );
            assert_eq!(success(&out), body);
            let requests = stub.finish();
            assert_eq!(requests.len(), 1);
            let path = requests[0].split_whitespace().nth(1).unwrap();
            let url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
            assert!(
                url.query_pairs()
                    .any(|(key, value)| key == "cursor" && value == cursor)
            );
        }
    }
}
#[test]
fn cursor_traversal_preserves_filter_and_returns_wire_array() {
    let stub = Stub::new(vec![
        response(
            "200 OK",
            page(json!([{"id":"a","word_count":null}]), Some("next+/=")),
            "",
        ),
        response("200 OK", page(json!([{"id":"b"}]), None), ""),
    ]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "stack",
            "list",
            "--all",
            "--limit",
            "1",
            "--filter",
            r#"{"where":{}}"#,
        ],
        None,
    );
    assert_eq!(success(&out), json!([{"id":"a","word_count":null},{"id":"b"}]));
    let requests = stub.finish();
    assert!(requests[1].contains("cursor=next%2B%2F%3D"));
    for request in requests {
        assert!(request.contains("filter=%7B%22where%22%3A%7B%7D%7D"));
        assert!(request.contains("limit=1"));
        assert!(!request.contains("page="));
    }
}
#[test]
fn one_page_preserves_server_envelope() {
    let body = page(json!([{"id":"a"}]), Some("next"));
    let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
    assert_eq!(
        success(&command(&stub.origin, &["--json", "style-guide", "list"], None)),
        body
    );
    stub.finish();
}
#[test]
fn key_name_update_sends_only_name_with_merge_patch_and_if_match() {
    let id = "ress_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let body = json!({"object_type":"KEY", "id":id, "key":"cell_A1", "name":"Product title"});
    let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--if-match",
            "\"key-version\"",
            "api",
            "call",
            "updateKey",
            "--path",
            &format!("keyId={id}"),
            "--file",
            "-",
        ],
        Some(r#"{"name":"Product title"}"#),
    );
    assert_eq!(success(&out), body);
    let requests = stub.finish();
    let raw = &requests[0];
    assert!(raw.starts_with(&format!("PATCH /v2/keys/{id} ")));
    assert_eq!(
        header_values(raw, "content-type"),
        ["application/merge-patch+json"]
    );
    assert!(raw.to_lowercase().contains("if-match: \"key-version\""));
    assert_eq!(
        serde_json::from_str::<Value>(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap(),
        json!({"name":"Product title"})
    );
}

#[test]
fn withdrawn_csv_mutations_are_not_available_as_named_operations() {
    for operation in [
        "createCsvRow",
        "addCsvColumn",
        "deleteCsvRow",
        "deleteCsvColumn",
        "setCsvRowOrder",
        "setCsvColumnOrder",
    ] {
        let out = command("http://127.0.0.1:1", &["api", "call", operation], None);
        assert!(!out.status.success());
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(
            error.contains("Unknown") || error.contains("unknown"),
            "{operation}: {error}"
        );
    }
}

#[test]
fn patch_preserves_null_omission_and_conditional_headers() {
    let stub = Stub::new(vec![response("200 OK", json!({"id":"pr_a","website":null}), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--file",
            "-",
            "--if-match",
            "\"revision\"",
            "--idempotency-key",
            "operation-1",
            "project",
            "update",
            "pr_a",
        ],
        Some(r#"{"website":null}"#),
    );
    success(&out);
    let requests = stub.finish();
    let raw = &requests[0];
    assert!(raw.starts_with("PATCH /v2/projects/pr_a "));
    assert_eq!(
        header_values(raw, "content-type"),
        ["application/merge-patch+json"]
    );
    assert!(raw.to_lowercase().contains("if-match: \"revision\""));
    assert!(raw.to_lowercase().contains("idempotency-key: operation-1"));
    assert_eq!(
        serde_json::from_str::<Value>(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap(),
        json!({"website":null})
    );
}
#[test]
fn operation_lookup_uses_real_path_and_query_encoding() {
    let stub = Stub::new(vec![response(
        "200 OK",
        json!({"variants":[{"plural_form":"ONE","content":"One"}]}),
        "",
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "getSegmentText",
            "--path",
            "segmentId=seg_abcd",
            "--path",
            "languageCode=de-AT",
            "--query",
            "custom=a&b",
        ],
        None,
    );
    success(&out);
    assert!(stub.finish()[0].starts_with("GET /v2/segments/seg_abcd/texts/de-AT?custom=a%26b "));
}
#[test]
fn structured_errors_keep_codes_pointers_request_id_and_retry_after() {
    let body = json!({"title":"Validation failed","status":422,"code":"validation_failed","errors":[{"pointer":"#/name","detail":"Required","code":"not_null"}]});
    let stub = Stub::new(vec![response(
        "422 Unprocessable Entity",
        body.clone(),
        "X-Request-Id: req-2\r\nRetry-After: 10\r\n",
    )]);
    let out = command(
        &stub.origin,
        &["--json", "api", "request", "POST", "/v2/stacks", "--file", "-"],
        Some("{}"),
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["problem"], body);
    assert_eq!(err["request_id"], "req-2");
    assert_eq!(err["retry_after"], "10");
    stub.finish();
}
#[test]
fn redirects_are_not_followed() {
    let trap = TcpListener::bind("127.0.0.1:0").unwrap();
    trap.set_nonblocking(true).unwrap();
    let stub = Stub::new(vec![response(
        "307 Temporary Redirect",
        json!({}),
        &format!("Location: http://{}/capture\r\n", trap.local_addr().unwrap()),
    )]);
    let out = command(&stub.origin, &["--json", "workspace"], None);
    assert_eq!(out.status.code(), Some(1));
    stub.finish();
    assert_eq!(trap.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}
#[test]
fn empty_delete_is_success_without_fabricated_json() {
    let stub = Stub::new(vec![
        "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".into(),
    ]);
    let out = command(&stub.origin, &["--json", "stack", "delete", "stack_abcd"], None);
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    assert!(stub.finish()[0].starts_with("DELETE /v2/stacks/stack_abcd "));
}
#[test]
fn pagination_cycles_and_invalid_envelopes_fail() {
    for bodies in [
        vec![
            page(json!([{"id":"a"}]), Some("same")),
            page(json!([{"id":"b"}]), Some("same")),
        ],
        vec![json!({"object_type":"LIST","data":[],"has_more":true,"next_cursor":null})],
    ] {
        let expected = if bodies.len() == 2 {
            "repeated a pagination cursor"
        } else {
            "collection"
        };
        let stub = Stub::new(bodies.into_iter().map(|b| response("200 OK", b, "")).collect());
        let out = command(&stub.origin, &["--json", "stack", "list", "--all"], None);
        assert_eq!(out.status.code(), Some(1));
        assert_error_contains(&out, expected);
        assert!(out.stdout.is_empty());
        stub.finish();
    }
}
#[test]
fn invalid_inputs_fail_before_network_access() {
    for args in [
        vec!["--json", "api", "request", "GET", "https://example.com"],
        vec!["--json", "api", "request", "GET", "/v2/../admin"],
        vec!["--json", "api", "call", "getStack", "--path", "stackId=x/y"],
        vec!["--json", "api", "call", "getStack"],
        vec!["--json", "project", "update", "pr_a"],
        vec!["--json", "project", "get", "a/b"],
    ] {
        let out = command("http://127.0.0.1:1", &args, None);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.is_empty());
    }
}
#[test]
fn inventory_exposes_all_bundled_operations_and_content_types() {
    let out = command("http://127.0.0.1:1", &["api", "operations"], None);
    let operations = success(&out);
    let spec: Value = serde_json::from_str(include_str!("../contract/v2.json")).unwrap();
    let expected: std::collections::BTreeSet<_> = spec["paths"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|item| item.as_object().unwrap().values())
        .filter_map(|op| op["operationId"].as_str())
        .collect();
    let actual: std::collections::BTreeSet<_> = operations
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["operation_id"].as_str().unwrap())
        .collect();
    assert_eq!(actual, expected);
    assert_eq!(operations.as_array().unwrap().len(), actual.len());
    for operation in [
        "getKey",
        "createKey",
        "updateText",
        "transitionText",
        "renameKey",
        "deleteKey",
    ] {
        assert!(actual.contains(operation));
    }
    assert!(
        operations
            .as_array()
            .unwrap()
            .iter()
            .any(|op| op["operation_id"] == "getText")
    );
}

#[test]
fn direct_json_fields_and_project_stack_id_are_preserved() {
    let stub = Stub::new(vec![response("201 Created", json!({"id":"pr_a"}), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "project",
            "create",
            "--field",
            r#"name="My project""#,
            "--field",
            r#"stack_id="stack_a""#,
            "--field",
            r#"target_language_codes=["de","fr"]"#,
            "--field",
            "ai_context=null",
        ],
        None,
    );
    success(&out);
    let requests = stub.finish();
    let body: Value = serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(
        body,
        json!({"name":"My project","stack_id":"stack_a","target_language_codes":["de","fr"],"ai_context":null})
    );
}
#[test]
fn writes_are_not_retried_on_service_failure() {
    let stub = Stub::serve(
        vec![
            response(
                "503 Service Unavailable",
                json!({"code":"service_unavailable"}),
                "Retry-After: 1\r\n",
            )
            .into_bytes(),
        ],
        |_| {},
        Duration::from_millis(1250),
    );
    let out = command(
        &stub.origin,
        &["--json", "stack", "create", "--file", "-"],
        Some("{}"),
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stub.finish().len(), 1);
}
#[test]
fn list_headers_are_emitted_for_each_page_on_stderr() {
    let stub = Stub::new(vec![
        response(
            "200 OK",
            page(json!([{"id":"a"}]), Some("next")),
            "X-Request-Id: page-1\r\n",
        ),
        response("200 OK", page(json!([]), None), "X-Request-Id: page-2\r\n"),
    ]);
    let out = command(
        &stub.origin,
        &["--json", "--include-headers", "stack", "list", "--all"],
        None,
    );
    assert_eq!(success(&out), json!([{"id":"a"}]));
    for id in ["page-1", "page-2"] {
        assert!(String::from_utf8_lossy(&out.stderr).contains(id));
    }
    stub.finish();
}
#[test]
fn malformed_json_and_duplicate_fields_do_not_send_requests() {
    for (args, body) in [
        (vec!["--json", "stack", "create", "--file", "-"], Some("{")),
        (
            vec![
                "--json", "stack", "create", "--field", "name=1", "--field", "name=2",
            ],
            None,
        ),
    ] {
        let out = command("http://127.0.0.1:1", &args, body);
        assert_eq!(out.status.code(), Some(2));
        assert!(out.stdout.is_empty());
    }
}

#[test]
fn idempotent_replay_header_uses_the_backend_header_name() {
    let stub = Stub::new(vec![response(
        "201 Created",
        json!({"id":"stack_a"}),
        "Idempotent-Replayed: true\r\n",
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "stack",
            "create",
            "--file",
            "-",
            "--idempotency-key",
            "create-1",
        ],
        Some("{}"),
    );
    success(&out);
    assert!(String::from_utf8_lossy(&out.stderr).contains("idempotent-replayed: true"));
    stub.finish();
}

#[test]
fn empty_intermediate_cursor_pages_are_followed() {
    let stub = Stub::new(vec![
        response("200 OK", page(json!([]), Some("next")), ""),
        response("200 OK", page(json!([{"id":"a"}]), None), ""),
    ]);
    assert_eq!(
        success(&command(
            &stub.origin,
            &["--json", "stack", "list", "--all"],
            None
        )),
        json!([{"id":"a"}])
    );
    assert_eq!(stub.finish().len(), 2);
}
#[test]
fn invalid_request_headers_are_input_errors() {
    for header in ["--if-match", "--idempotency-key"] {
        let out = command(
            "http://127.0.0.1:1",
            &[
                "--json",
                "api",
                "request",
                "PATCH",
                "/v2/projects/pr_a",
                header,
                "bad\r\nheader",
                "--file",
                "-",
            ],
            Some("{}"),
        );
        assert_eq!(
            out.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.is_empty());
    }
}
#[test]
fn human_error_prints_request_id_once_with_header_fallback() {
    for body in [
        json!({"title":"Denied","request_id":"req-one"}),
        json!({"title":"Denied"}),
    ] {
        let stub = Stub::new(vec![response("403 Forbidden", body, "X-Request-Id: req-one\r\n")]);
        let out = command(&stub.origin, &["workspace"], None);
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(
            String::from_utf8_lossy(&out.stderr)
                .matches("request_id: req-one")
                .count(),
            1
        );
        stub.finish();
    }
}

#[test]
fn key_operations_preserve_public_paths_conditions_and_key_json() {
    let id = "ress_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let path_arg = format!("keyId={id}");
    let body = json!({"key": "navigation.\"literal.dot\"", "group_key": null});
    let stub = Stub::new(vec![
        response("200 OK", body.clone(), "ETag: \"key-version\"\r\n"),
        response("200 OK", body.clone(), "ETag: \"renamed-version\"\r\n"),
        "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".into(),
    ]);
    let read = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "getKey",
            "--path",
            &path_arg,
        ],
        None,
    );
    assert_eq!(success(&read), body);
    assert!(String::from_utf8_lossy(&read.stderr).contains("etag: \"key-version\""));
    let write = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "--if-match",
            "\"key-version\"",
            "api",
            "call",
            "renameKey",
            "--path",
            &path_arg,
            "--file",
            "-",
        ],
        Some(&body.to_string()),
    );
    assert_eq!(success(&write), body);
    assert!(String::from_utf8_lossy(&write.stderr).contains("etag: \"renamed-version\""));
    let delete = command(
        &stub.origin,
        &[
            "--json",
            "--if-match",
            "\"renamed-version\"",
            "api",
            "call",
            "deleteKey",
            "--path",
            &path_arg,
        ],
        None,
    );
    assert!(delete.status.success());
    assert!(delete.stdout.is_empty());
    let requests = stub.finish();
    assert!(requests[0].starts_with(&format!("GET /v2/keys/{id} ")));
    assert!(requests[1].starts_with(&format!("POST /v2/keys/{id}/rename ")));
    assert!(requests[2].starts_with(&format!("DELETE /v2/keys/{id} ")));
    assert!(requests[1].to_lowercase().contains("if-match: \"key-version\""));
    assert!(
        requests[2]
            .to_lowercase()
            .contains("if-match: \"renamed-version\"")
    );
    let written: Value = serde_json::from_str(requests[1].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(written, body);
}

#[test]
fn text_operation_preserves_structured_variants_clears_conditions_and_field_errors() {
    let id = "txt_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let path_arg = format!("textId={id}");
    let context = "ress_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let body = json!({"key_id": context, "is_minor_update": null, "variants": [
        {"device":"OTHER", "plural_form":"ONE", "content":"One \"item\" {name}"},
        {"device":"PHONE", "plural_form":"OTHER", "content":"Many\nitems"}
    ]});
    let clear = json!({"key_id": context, "variants": []});
    let problem = json!({"status":422,"code":"validation_failed","errors":[
        {"pointer":"#/variants/0/content","code":"type","detail":"must not be null"}
    ]});
    let stub = Stub::new(vec![
        response("200 OK", body.clone(), "ETag: \"new-text\"\r\n"),
        response("200 OK", clear.clone(), ""),
        response("422 Unprocessable Entity", problem.clone(), ""),
    ]);
    let written = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "--if-match",
            "\"old-text\"",
            "api",
            "call",
            "updateText",
            "--path",
            &path_arg,
            "--file",
            "-",
        ],
        Some(&body.to_string()),
    );
    assert_eq!(success(&written), body);
    assert!(String::from_utf8_lossy(&written.stderr).contains("etag: \"new-text\""));
    let cleared = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "updateText",
            "--path",
            &path_arg,
            "--file",
            "-",
        ],
        Some(&clear.to_string()),
    );
    assert_eq!(success(&cleared), clear);
    let rejected = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "updateText",
            "--path",
            &path_arg,
            "--file",
            "-",
        ],
        Some("{\"variants\":[{\"content\":null}]}"),
    );
    assert_eq!(rejected.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&rejected.stderr).unwrap();
    assert_eq!(error["problem"], problem);
    let requests = stub.finish();
    for request in &requests {
        assert!(request.starts_with(&format!("PATCH /v2/texts/{id} ")));
    }
    assert!(requests[0].to_lowercase().contains("if-match: \"old-text\""));
    let actual: Value = serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(actual, body);
    let actual_clear: Value = serde_json::from_str(requests[1].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(actual_clear, clear);
}

#[test]
fn named_text_transition_preserves_actions_conditions_replay_and_field_errors() {
    let id = "txt_019b76da-c243-7000-8000-000000000701";
    let path_arg = format!("textId={id}");
    let actions = [
        ("APPROVE", "APPROVED"),
        ("REOPEN", "DRAFT"),
        ("MARK_TRANSLATED", "TRANSLATED"),
        ("REJECT", "REJECTED"),
    ];
    let mut responses: Vec<String> = actions
        .iter()
        .map(|(_, status)| {
            response(
                "200 OK",
                json!({"id":id,"status":status,"variants":[]}),
                "ETag: \"current\"\r\n",
            )
        })
        .collect();
    responses.push(response(
        "200 OK",
        json!({"id":id,"status":"APPROVED","variants":[]}),
        "ETag: \"replayed\"\r\nIdempotent-Replayed: true\r\n",
    ));
    let invalid = json!({"status":422,"code":"validation_failed","errors":[{"pointer":"#/action","code":"not_supported"}]});
    let stale = json!({"status":412,"code":"precondition_failed"});
    responses.push(response("422 Unprocessable Entity", invalid.clone(), ""));
    responses.push(response("412 Precondition Failed", stale.clone(), ""));
    let stub = Stub::new(responses);
    for (action, status) in actions {
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                "transitionText",
                "--path",
                &path_arg,
                "--if-match",
                "\"before\"",
                "--idempotency-key",
                action,
                "--file",
                "-",
            ],
            Some(&json!({"action":action}).to_string()),
        );
        assert_eq!(success(&out), json!({"id":id,"status":status,"variants":[]}));
        assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"current\""));
    }
    let replay = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "transitionText",
            "--path",
            &path_arg,
            "--if-match",
            "\"before\"",
            "--idempotency-key",
            "APPROVE",
            "--file",
            "-",
        ],
        Some(r#"{"action":"APPROVE"}"#),
    );
    assert_eq!(success(&replay)["status"], "APPROVED");
    let headers = String::from_utf8_lossy(&replay.stderr);
    assert!(headers.contains("idempotent-replayed: true"));
    assert!(headers.contains("etag: \"replayed\""));
    for (action, expected) in [("MARK_TRANSLATED", invalid), ("REOPEN", stale)] {
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "transitionText",
                "--path",
                &path_arg,
                "--if-match",
                "\"before\"",
                "--file",
                "-",
            ],
            Some(&json!({"action":action}).to_string()),
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            expected
        );
    }
    let requests = stub.finish();
    assert_eq!(requests.len(), 7);
    for (i, request) in requests.iter().enumerate() {
        assert!(request.starts_with(&format!("POST /v2/texts/{id}/transition HTTP/1.1")));
        assert!(request.to_lowercase().contains("content-type: application/json"));
        assert!(request.to_lowercase().contains("if-match: \"before\""));
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let expected = [
            "APPROVE",
            "REOPEN",
            "MARK_TRANSLATED",
            "REJECT",
            "APPROVE",
            "MARK_TRANSLATED",
            "REOPEN",
        ][i];
        assert_eq!(body, json!({"action":expected}));
        if i < 5 {
            assert!(
                request
                    .to_lowercase()
                    .contains(&format!("idempotency-key: {}", expected.to_lowercase()))
            );
        }
    }
}

#[test]
fn named_resource_creation_preserves_explicit_template_and_created_location() {
    let branch = "prb_019b76da-c248-7000-8000-000000000001";
    let resource = "rc_019b76da-c248-7000-8000-000000000002";
    let location = format!("/v2/resources/{resource}");
    let body = json!({"path":"./locales/Grüße/%{locale}.json", "format":"STANDARD_JSON", "language_code_separator":"-"});
    let created = json!({"object_type":"RESOURCE", "id":resource, "type":"FILE", "path":"./locales/Grüße/%{locale}.json"});
    let stub = Stub::new(vec![response(
        "201 Created",
        created.clone(),
        &format!("Location: {location}\r\nIdempotent-Replayed: true\r\n"),
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "createResource",
            "--path",
            &format!("branchId={branch}"),
            "--idempotency-key",
            "create-resource",
            "--file",
            "-",
        ],
        Some(&body.to_string()),
    );
    assert_eq!(success(&out), created);
    let headers = String::from_utf8_lossy(&out.stderr);
    assert!(headers.contains(&format!("location: {location}")));
    assert!(headers.contains("idempotent-replayed: true"));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!("POST /v2/branches/{branch}/resources ")));
    assert!(
        requests[0]
            .to_lowercase()
            .contains("idempotency-key: create-resource")
    );
    let submitted: Value = serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(submitted, body);
}

#[test]
fn resource_creation_reports_path_format_and_conflict_errors_without_retry() {
    for (status, code) in [
        ("422 Unprocessable Entity", "validation_failed"),
        ("409 Conflict", "already_exists"),
        ("409 Conflict", "conflict"),
        ("409 Conflict", "branch_not_ready"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let problem = json!({"title":"Resource creation rejected","code":code,"request_id":"create-resource-request","errors":[{"pointer":"#/path","code":"pattern","detail":"Invalid path"}]});
        let stub = Stub::new(vec![response(status, problem, "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "createResource",
                "--path",
                "branchId=prb_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--file",
                "-",
            ],
            Some(
                "{\"path\":\"%{locale}.json\",\"format\":\"STANDARD_JSON\",\"language_code_separator\":\"-\"}",
            ),
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(error.contains(code));
        assert!(error.contains("create-resource-request"));
        assert!(error.contains("#/path"));
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn named_key_creation_preserves_structured_body_and_created_location() {
    let resource = "rc_019b76da-c248-7000-8000-000000000001";
    let key_id = "ress_019b76da-c248-7000-8000-000000000002";
    let location = format!("/v2/keys/{key_id}");
    let body = json!({
        "key": "fruit", "is_plural": true,
        "source_variants": [
            {"device":"PHONE", "plural_form":"ONE", "content":"One apple"},
            {"device":"TABLET", "plural_form":"OTHER", "content":"Apples"}
        ],
        "group_key": null, "label_ids": null, "ai_context": "Fruit counter"
    });
    let created = json!({"object_type":"KEY", "id":key_id, "is_plural":true});
    let stub = Stub::new(vec![response(
        "201 Created",
        created.clone(),
        &format!("Location: {location}\r\nIdempotent-Replayed: true\r\n"),
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "createKey",
            "--path",
            &format!("resourceId={resource}"),
            "--idempotency-key",
            "create-fruit",
            "--file",
            "-",
        ],
        Some(&body.to_string()),
    );
    assert_eq!(success(&out), created);
    let headers = String::from_utf8_lossy(&out.stderr);
    assert!(headers.contains(&format!("location: {location}")));
    assert!(headers.contains("idempotent-replayed: true"));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!("POST /v2/resources/{resource}/keys ")));
    assert!(
        requests[0]
            .to_lowercase()
            .contains("idempotency-key: create-fruit")
    );
    let submitted: Value = serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(submitted, body);
}

fn bulk_input(result: &Value) -> Value {
    let items = result.get("data").and_then(Value::as_array).filter(|data| !data.is_empty())
        .map(|data| data.iter().enumerate().map(|(index, item)| json!({"reference": item.get("reference").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| format!("item-{index}")), "key":{"key":format!("key_{index}"),"is_plural":false,"source_variants":[]}})).collect::<Vec<_>>())
        .unwrap_or_else(|| vec![json!({"reference":"item-0","key":{"key":"key_0","is_plural":false,"source_variants":[]}})]);
    json!({"items":items})
}

fn bulk_result(outcomes: &[&str]) -> Value {
    json!({"object_type":"BULK_RESULT","data":outcomes.iter().enumerate().map(|(index, outcome)| {
        let status = match *outcome { "SUCCEEDED" => json!(201), "FAILED" => json!(422), "UNKNOWN" => json!(500), _ => Value::Null };
        json!({"index":index,"reference":format!("item-{index}"),"outcome":outcome, "status":status,
            "data":if *outcome == "SUCCEEDED" { json!({"object_type":"KEY","id":"ress_public"}) } else { Value::Null },
            "location":if *outcome == "SUCCEEDED" { json!("/v2/keys/ress_public") } else { Value::Null },
            "etag":if *outcome == "SUCCEEDED" { json!("\"version\"") } else { Value::Null },
            "error":if matches!(*outcome, "FAILED" | "UNKNOWN") { json!({"status":status,"code":"example_problem"}) } else { Value::Null }})
    }).collect::<Vec<_>>()})
}

#[test]
fn bulk_success_preserves_ordered_results_and_sends_one_named_request() {
    let body = bulk_result(&["SUCCEEDED", "SUCCEEDED"]);
    let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
    let input = bulk_input(&body).to_string();
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "createKeysBulk",
            "--path",
            "resourceId=rc_example",
            "--idempotency-key",
            "bulk-one",
            "--file",
            "-",
        ],
        Some(&input),
    );
    assert_eq!(success(&out), body);
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("POST /v2/resources/rc_example/keys/bulk-create "));
    assert!(requests[0].to_lowercase().contains("idempotency-key: bulk-one"));
    assert_eq!(
        serde_json::from_str::<Value>(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap(),
        serde_json::from_str::<Value>(&input).unwrap()
    );
}

#[test]
fn bulk_partial_failure_preserves_full_stdout_and_reports_structured_summary() {
    let mut body = bulk_result(&["SUCCEEDED", "FAILED", "SUCCEEDED"]);
    body["data"][0]["etag"] = json!("\"created-version\"");
    body["data"][0]["location"] = json!("/v2/keys/ress_public");
    body["data"][1]["status"] = json!(422);
    body["data"][1]["error"] = json!({"status":422,"code":"validation_failed","errors":[{"pointer":"#/items/1/key/key","code":"not_supported","detail":"Invalid key"}],"future_field":{"keep":true}});
    let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "request",
            "POST",
            "/v2/resources/rc_example/keys/bulk-create",
            "--file",
            "-",
        ],
        Some(&bulk_input(&body).to_string()),
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
    let summary: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(summary["error"], "bulk_incomplete");
    assert!(
        summary["detail"]
            .as_str()
            .unwrap()
            .contains("Successful items are already committed")
    );
    assert_eq!(summary["succeeded"], 2);
    assert_eq!(summary["failed"], 1);
    assert_eq!(summary["unknown"], 0);
    assert_eq!(stub.finish().len(), 1);
}

#[test]
fn bulk_uncertain_future_and_unprocessed_outcomes_fail_without_retrying() {
    for outcome in ["UNKNOWN", "NOT_PROCESSED", "FUTURE_OUTCOME"] {
        let body = bulk_result(&["SUCCEEDED", outcome]);
        let stub = Stub::new(vec![response(
            "200 OK",
            body.clone(),
            "Idempotent-Replayed: true\r\n",
        )]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                "createKeysBulk",
                "--path",
                "resourceId=rc_example",
                "--file",
                "-",
            ],
            Some(&bulk_input(&body).to_string()),
        );
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("idempotent-replayed: true"));
        assert!(stderr.contains("bulk_incomplete"));
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn malformed_bulk_envelopes_fail_after_preserving_the_response() {
    for body in [
        json!({"object_type":"BULK_RESULT"}),
        json!({"object_type":"BULK_RESULT","data":{}}),
        bulk_result(&[]),
        json!({"object_type":"BULK_RESULT","data":[{}]}),
        json!({"object_type":"BULK_RESULT","data":[{"outcome":42}]}),
    ] {
        let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "createKeysBulk",
                "--path",
                "resourceId=rc_example",
                "--file",
                "-",
            ],
            Some(&bulk_input(&body).to_string()),
        );
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        assert!(String::from_utf8_lossy(&out.stderr).contains("Malformed BULK_RESULT"));
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn human_bulk_output_keeps_each_item_and_reports_failure_separately() {
    let body = bulk_result(&["SUCCEEDED", "NOT_PROCESSED"]);
    let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "api",
            "call",
            "createKeysBulk",
            "--path",
            "resourceId=rc_example",
            "--file",
            "-",
        ],
        Some(&bulk_input(&body).to_string()),
    );
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("reference: item-0"));
    assert!(stdout.contains("reference: item-1"));
    assert!(stdout.contains("outcome: NOT_PROCESSED"));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("1 succeeded, 0 failed, 0 unknown, 1 not processed")
    );
    assert_eq!(stub.finish().len(), 1);
}

#[test]
fn known_bulk_calls_reject_missing_or_wrong_discriminator_and_empty_bodies() {
    for named in [true, false] {
        for body in [
            json!({"data":[{"outcome":"FAILED"}]}),
            json!({"object_type":"LIST","data":[{"outcome":"UNKNOWN"}]}),
            Value::Null,
        ] {
            let wire = if body.is_null() {
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
            } else {
                response("200 OK", body.clone(), "")
            };
            let stub = Stub::new(vec![wire]);
            let args = if named {
                vec![
                    "--json",
                    "api",
                    "call",
                    "createKeysBulk",
                    "--path",
                    "resourceId=rc_example",
                    "--file",
                    "-",
                ]
            } else {
                vec![
                    "--json",
                    "api",
                    "request",
                    "POST",
                    "/v2/resources/rc_example/keys/bulk-create",
                    "--file",
                    "-",
                ]
            };
            let out = command(&stub.origin, &args, Some(&bulk_input(&body).to_string()));
            assert_eq!(out.status.code(), Some(1));
            if body.is_null() {
                assert!(out.stdout.is_empty());
            } else {
                assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
            }
            assert!(String::from_utf8_lossy(&out.stderr).contains("BULK_RESULT"));
            assert_eq!(stub.finish().len(), 1);
        }
    }
}

#[test]
fn bulk_results_must_match_every_submitted_index_and_reference() {
    let input = json!({"items":[{"reference":"item-0"},{"reference":"item-1"}]});
    for variant in ["missing", "index", "reference"] {
        let mut body = bulk_result(&["SUCCEEDED", "SUCCEEDED"]);
        match variant {
            "missing" => {
                body["data"].as_array_mut().unwrap().pop();
            }
            "index" => body["data"][1]["index"] = json!(0),
            _ => body["data"][1]["reference"] = json!("wrong"),
        }
        let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "createKeysBulk",
                "--path",
                "resourceId=rc_example",
                "--file",
                "-",
            ],
            Some(&input.to_string()),
        );
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        assert!(String::from_utf8_lossy(&out.stderr).contains("BULK_RESULT"));
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn bulk_success_requires_result_fields_and_their_success_types() {
    for field in ["data", "error", "etag", "location", "status"] {
        for missing in [true, false] {
            let mut body = bulk_result(&["SUCCEEDED"]);
            if missing {
                body["data"][0].as_object_mut().unwrap().remove(field);
            } else {
                body["data"][0][field] = json!(false);
            }
            let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
            let out = command(
                &stub.origin,
                &[
                    "--json",
                    "api",
                    "call",
                    "createKeysBulk",
                    "--path",
                    "resourceId=rc_example",
                    "--file",
                    "-",
                ],
                Some(&bulk_input(&body).to_string()),
            );
            assert_eq!(out.status.code(), Some(1));
            assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
            assert!(String::from_utf8_lossy(&out.stderr).contains("Malformed BULK_RESULT"));
            assert_eq!(stub.finish().len(), 1);
        }
    }
}

#[test]
fn bulk_creation_success_requires_201_and_a_key_discriminator() {
    for (status, object_type) in [
        (200, "KEY"),
        (202, "KEY"),
        (204, "KEY"),
        (201, "TEXT"),
        (201, "SEGMENT"),
        (201, "FUTURE"),
    ] {
        let mut body = bulk_result(&["SUCCEEDED"]);
        body["data"][0]["status"] = json!(status);
        body["data"][0]["data"]["object_type"] = json!(object_type);
        let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "createKeysBulk",
                "--path",
                "resourceId=rc_example",
                "--file",
                "-",
            ],
            Some(&bulk_input(&body).to_string()),
        );
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        assert!(String::from_utf8_lossy(&out.stderr).contains("Malformed BULK_RESULT"));
        assert_eq!(stub.finish().len(), 1);
    }
}

fn text_batch_input(workflow: bool) -> Value {
    json!({"items":[if workflow {
        json!({"reference":"item-0","text_id":"txt_example","key_id":"ress_example","action":"APPROVE","if_match":"\"current\""})
    } else {
        json!({"reference":"item-0","text_id":"txt_example","if_match":"\"current\"","update":{"key_id":"ress_example","variants":[{"device":"PHONE","plural_form":"OTHER","content":"Updated"}],"is_minor_update":true}})
    }]})
}

fn text_batch_result(outcomes: &[&str]) -> Value {
    let mut body = bulk_result(outcomes);
    for item in body["data"].as_array_mut().unwrap() {
        if item["outcome"] == "SUCCEEDED" {
            item["status"] = json!(200);
            item["data"] = json!({"id":"txt_example","object_type":"TEXT","status":"APPROVED","variants":[]});
            item["location"] = json!("/v2/texts/txt_example");
        }
    }
    body
}

#[test]
fn text_batches_preserve_named_and_raw_paths_bodies_and_item_conditions() {
    for workflow in [false, true] {
        let operation = if workflow {
            "transitionTextsBulk"
        } else {
            "updateTextsBulk"
        };
        let path = if workflow {
            "/v2/branches/prb_example/texts/bulk-transition"
        } else {
            "/v2/branches/prb_example/texts/bulk-update"
        };
        for named in [false, true] {
            let body = text_batch_result(&["SUCCEEDED"]);
            let input = text_batch_input(workflow);
            let stub = Stub::new(vec![response(
                "200 OK",
                body.clone(),
                "Idempotent-Replayed: true\r\n",
            )]);
            let mut args = vec!["--json", "--include-headers", "api"];
            if named {
                args.extend(["call", operation, "--path", "branchId=prb_example"]);
            } else {
                args.extend(["request", "POST", path]);
            }
            args.extend(["--idempotency-key", "text-batch", "--file", "-"]);
            let out = command(&stub.origin, &args, Some(&input.to_string()));
            assert_eq!(success(&out), body);
            assert!(String::from_utf8_lossy(&out.stderr).contains("idempotent-replayed: true"));
            let requests = stub.finish();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].starts_with(&format!("POST {path} ")));
            assert!(requests[0].to_lowercase().contains("idempotency-key: text-batch"));
            assert!(!requests[0].to_lowercase().contains("\r\nif-match:"));
            assert_eq!(
                serde_json::from_str::<Value>(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap(),
                input
            );
        }
    }
}

#[test]
fn text_batches_reject_wrong_success_status_and_resource_kind() {
    for operation in ["updateTextsBulk", "transitionTextsBulk"] {
        for (status, kind) in [(201, "TEXT"), (202, "TEXT"), (200, "KEY"), (200, "TRANSLATION")] {
            let mut body = text_batch_result(&["SUCCEEDED"]);
            body["data"][0]["status"] = json!(status);
            body["data"][0]["data"]["object_type"] = json!(kind);
            let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
            let out = command(
                &stub.origin,
                &[
                    "--json",
                    "api",
                    "call",
                    operation,
                    "--path",
                    "branchId=prb_example",
                    "--file",
                    "-",
                ],
                Some(&text_batch_input(operation == "transitionTextsBulk").to_string()),
            );
            assert_eq!(out.status.code(), Some(1));
            assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
            assert!(String::from_utf8_lossy(&out.stderr).contains("Malformed BULK_RESULT"));
            assert_eq!(stub.finish().len(), 1);
        }
    }
}

#[test]
fn text_batch_partial_problems_remain_visible_without_retry() {
    for workflow in [false, true] {
        let operation = if workflow {
            "transitionTextsBulk"
        } else {
            "updateTextsBulk"
        };
        let mut input = text_batch_input(workflow);
        let mut second = input["items"][0].clone();
        second["reference"] = json!("item-1");
        second["text_id"] = json!("txt_second");
        input["items"].as_array_mut().unwrap().push(second);
        let mut body = text_batch_result(&["SUCCEEDED", "FAILED"]);
        body["data"][1]["status"] = json!(412);
        body["data"][1]["error"] =
            json!({"status":412,"code":"precondition_failed","request_id":"trace-bulk"});
        let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                operation,
                "--path",
                "branchId=prb_example",
                "--file",
                "-",
            ],
            Some(&input.to_string()),
        );
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        let summary: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(summary["error"], "bulk_incomplete");
        assert_eq!(summary["succeeded"], 1);
        assert_eq!(summary["failed"], 1);
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn lina_discovery_preserves_named_paths_pages_and_nullable_configuration() {
    for (list, get, collection, parameter, item) in [
        (
            "listLinas",
            "getLina",
            "linas",
            "linaId",
            json!({
              "color": "#C084FC",
              "context": {
                "is_glossary_enabled": true,
                "is_labels_enabled": true,
                "is_segments_enabled": true,
                "is_style_guide_enabled": true,
                "is_translation_memory_enabled": true
              },
              "context_selection_mode": "AUTO",
              "created_at": "2026-09-29T10:00:00.123Z",
              "description": null,
              "engine_mode": "CUSTOMER_MANAGED",
              "glossary_id": null,
              "id": "lina_0197c8f2-5a6b-7cde-8f12-3456789abcde",
              "is_archived": false,
              "is_disabled": false,
              "is_restricted": false,
              "is_tm_match_enabled": true,
              "language_engine_overrides": [],
              "linguistic_engine_id": "engine_0197c8f2-5a6b-7cde-8f12-3456789abcde",
              "name": "Localization assistant",
              "object_type": "LINA",
              "restricted_stack_ids": [
                "stack_0197c8f2-5a6b-7cde-8f12-3456789abcde"
              ],
              "reviewer_linguistic_engine_id": null,
              "style_guide_id": null,
              "updated_at": "2026-09-29T10:00:00.123Z"
            }),
        ),
        (
            "listLinguisticEngines",
            "getLinguisticEngine",
            "linguistic-engines",
            "linguisticEngineId",
            json!({
              "created_at": "2026-09-29T10:00:00.123Z",
              "id": "engine_0197c8f2-5a6b-7cde-8f12-3456789abcde",
              "is_enabled": false,
              "is_glossary_context_supported": true,
              "is_labels_context_supported": true,
              "is_segments_context_supported": true,
              "is_style_guide_context_supported": true,
              "is_translation_memory_context_supported": true,
              "linguistic_credits_multiplier_basis_points": 10000,
              "model": "MT_GOOGLE_TRANSLATE_V3",
              "object_type": "LINGUISTIC_ENGINE",
              "provider": "GOOGLE",
              "type": "MT",
              "updated_at": "2026-09-29T10:00:00.123Z"
            }),
        ),
    ] {
        let id = item["id"].as_str().unwrap();
        let first_page = page(json!([item.clone()]), Some("cursor+next/="));
        let stub = Stub::new(vec![response("200 OK", first_page.clone(), "")]);
        let out = command(
            &stub.origin,
            &["--json", "api", "call", list, "--query", "limit=1"],
            None,
        );
        assert_eq!(success(&out), first_page);
        assert!(stub.finish()[0].starts_with(&format!("GET /v2/{collection}?limit=1 ")));
        let last_page = page(json!([]), None);
        let stub = Stub::new(vec![response("200 OK", last_page.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                list,
                "--query",
                "limit=1",
                "--query",
                "cursor=cursor+next/=",
            ],
            None,
        );
        assert_eq!(success(&out), last_page);
        let requests = stub.finish();
        assert!(requests[0].contains("cursor=cursor%2Bnext%2F%3D"));
        let stub = Stub::new(vec![response("200 OK", item.clone(), "ETag: \"discovery\"\r\n")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                get,
                "--path",
                &format!("{parameter}={id}"),
            ],
            None,
        );
        assert_eq!(success(&out), item);
        assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"discovery\""));
        assert!(stub.finish()[0].starts_with(&format!("GET /v2/{collection}/{id} ")));
    }
}

#[test]
fn webhook_discovery_preserves_scoped_paths_pages_and_delivery_outcomes() {
    let endpoint_id = "whe_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let delivery_id = "whd_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let endpoint = json!({
        "object_type": "WEBHOOK_ENDPOINT", "id": endpoint_id,
        "url": "https://example.com/lingohub/webhooks", "description": null,
        "status": "DISABLED", "event_types": ["project.created", "ping"],
        "stack_id": null, "project_id": null, "api_version": "2026-01-01",
        "consecutive_failures": 8, "last_success_at": null,
        "last_failure_at": "2026-09-29T10:00:00.123Z",
        "disabled_at": "2026-09-29T10:00:00.123Z",
        "created_at": "2026-09-29T09:00:00.123Z", "updated_at": "2026-09-29T10:00:00.123Z"
    });
    let delivery = json!({
        "object_type": "WEBHOOK_DELIVERY", "id": delivery_id,
        "webhook_endpoint_id": endpoint_id, "event_id": null, "event_type": null,
        "status": "FAILED", "is_exhausted": true, "attempt_count": 8, "max_attempts": 8,
        "next_attempt_at": null, "last_response_status": 503,
        "last_response_body": "Temporarily unavailable", "last_duration_ms": 120,
        "last_error": "non-2xx response: 503", "delivered_at": null,
        "created_at": "2026-09-29T09:00:00.123Z", "updated_at": "2026-09-29T10:00:00.123Z"
    });
    for (list, get, list_path, item_path, list_parameter, item_parameter, item) in [
        (
            "listWebhookEndpoints",
            "getWebhookEndpoint",
            "/v2/webhook-endpoints".to_string(),
            format!("/v2/webhook-endpoints/{endpoint_id}"),
            None,
            format!("webhookEndpointId={endpoint_id}"),
            endpoint,
        ),
        (
            "listWebhookDeliveries",
            "getWebhookDelivery",
            format!("/v2/webhook-endpoints/{endpoint_id}/deliveries"),
            format!("/v2/webhook-deliveries/{delivery_id}"),
            Some(format!("webhookEndpointId={endpoint_id}")),
            format!("webhookDeliveryId={delivery_id}"),
            delivery,
        ),
    ] {
        let first_page = page(json!([item.clone()]), Some("webhook+next/="));
        let stub = Stub::new(vec![response("200 OK", first_page.clone(), "")]);
        let mut args = vec!["--json", "api", "call", list, "--query", "limit=1"];
        if let Some(ref parent) = list_parameter {
            args.extend(["--path", parent]);
        }
        assert_eq!(success(&command(&stub.origin, &args, None)), first_page);
        assert!(stub.finish()[0].starts_with(&format!("GET {list_path}?limit=1 ")));
        let final_page = page(json!([]), None);
        let stub = Stub::new(vec![response("200 OK", final_page.clone(), "")]);
        args.extend(["--query", "cursor=webhook+next/="]);
        assert_eq!(success(&command(&stub.origin, &args, None)), final_page);
        let requests = stub.finish();
        assert!(requests[0].starts_with(&format!("GET {list_path}?")));
        assert!(requests[0].contains("cursor=webhook%2Bnext%2F%3D"));
        let stub = Stub::new(vec![response("200 OK", item.clone(), "ETag: \"webhook\"\r\n")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                get,
                "--path",
                &item_parameter,
            ],
            None,
        );
        assert_eq!(success(&out), item);
        assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"webhook\""));
        assert!(stub.finish()[0].starts_with(&format!("GET {item_path} ")));
    }
}

#[test]
fn webhook_lifecycle_preserves_bodyless_methods_conditions_and_empty_delete() {
    let id = "whe_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    for (operation, suffix, status) in [
        ("enableWebhookEndpoint", "enable", "ENABLED"),
        ("disableWebhookEndpoint", "disable", "DISABLED"),
    ] {
        let body = json!({
            "object_type": "WEBHOOK_ENDPOINT", "id": id, "url": "https://example.com/hooks",
            "description": null, "status": status, "event_types": ["*"], "stack_id": null,
            "project_id": null, "api_version": "2026-01-01", "consecutive_failures": 0,
            "last_success_at": null, "last_failure_at": null,
            "disabled_at": if status == "DISABLED" { Some("2026-09-29T10:00:00.123Z") } else { None },
            "created_at": "2026-09-29T09:00:00.123Z", "updated_at": "2026-09-29T10:00:00.123Z"
        });
        let stub = Stub::new(vec![response("200 OK", body.clone(), "ETag: \"updated\"\r\n")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                operation,
                "--path",
                &format!("webhookEndpointId={id}"),
                "--if-match",
                "\"current\"",
                "--idempotency-key",
                "webhook-change",
            ],
            None,
        );
        assert_eq!(success(&out), body);
        assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"updated\""));
        let requests = stub.finish();
        assert!(requests[0].starts_with(&format!("POST /v2/webhook-endpoints/{id}/{suffix} ")));
        assert!(requests[0].to_ascii_lowercase().contains("if-match: \"current\""));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("idempotency-key: webhook-change")
        );
        assert_eq!(requests[0].split_once("\r\n\r\n").unwrap().1, "");
    }
    let stub = Stub::new(vec![
        "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".into(),
    ]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "deleteWebhookEndpoint",
            "--path",
            &format!("webhookEndpointId={id}"),
            "--if-match",
            "\"current\"",
        ],
        None,
    );
    assert!(out.status.success());
    assert!(out.stdout.is_empty());
    let requests = stub.finish();
    assert!(requests[0].starts_with(&format!("DELETE /v2/webhook-endpoints/{id} ")));
    assert!(requests[0].to_ascii_lowercase().contains("if-match: \"current\""));
}

#[test]
fn webhook_catalog_preserves_reference_items_filters_and_cursors() {
    let item = json!({
        "object_type": "WEBHOOK_EVENT_TYPE", "event_type": "project.created",
        "name": "Project created", "category": "PROJECTS_AND_BRANCHES"
    });
    let first =
        json!({"object_type":"LIST", "has_more":true, "next_cursor":"catalog+/=", "data":[item.clone()]});
    let last = json!({"object_type":"LIST", "has_more":false, "next_cursor":null, "data":[]});
    let stub = Stub::new(vec![
        response("200 OK", first.clone(), ""),
        response("200 OK", last.clone(), ""),
    ]);
    let filter = r#"{"where":{"_and":[{"webhook_event_type[category]":{"_eq":"PROJECTS_AND_BRANCHES"}}]}}"#;
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listWebhookEventTypes",
            "--query",
            "limit=1",
            "--query",
            &format!("filter={filter}"),
        ],
        None,
    );
    assert_eq!(success(&out), first);
    let next = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listWebhookEventTypes",
            "--query",
            "limit=1",
            "--query",
            &format!("filter={filter}"),
            "--query",
            "cursor=catalog+/=",
        ],
        None,
    );
    assert_eq!(success(&next), last);
    let requests = stub.finish();
    for request in &requests {
        assert!(request.starts_with("GET /v2/webhook-event-types?"));
        let target = request.split_whitespace().nth(1).unwrap();
        let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "filter" && value == filter)
        );
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "limit" && value == "1")
        );
    }
    let target = requests[1].split_whitespace().nth(1).unwrap();
    let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "cursor" && value == "catalog+/=")
    );
    assert!(item.get("id").is_none());
    assert!(item.get("created_at").is_none());
}

#[test]
fn webhook_delivery_actions_preserve_job_location_and_retry_headers() {
    let endpoint = "whe_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let delivery = "whd_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let location = format!("/v2/webhook-deliveries/{delivery}");
    let item = json!({
        "object_type": "WEBHOOK_DELIVERY", "id": delivery, "webhook_endpoint_id": endpoint,
        "event_id": "evt_0197c8f2-5a6b-7cde-8f12-3456789abcde", "event_type": "ping",
        "status": "QUEUED", "is_exhausted": false, "attempt_count": 8, "max_attempts": 8,
        "next_attempt_at": "2026-09-29T10:00:00.123Z", "last_response_status": 503,
        "last_response_body": "Temporarily unavailable", "last_duration_ms": 120,
        "last_error": "non-2xx response: 503", "delivered_at": null,
        "created_at": "2026-09-29T09:00:00.123Z", "updated_at": "2026-09-29T10:00:00.123Z"
    });
    let job = json!({"object_type":"JOB", "job_type":"WEBHOOK_DELIVERY", "data":item.clone()});
    for (operation, parameter, path) in [
        (
            "testWebhookEndpoint",
            format!("webhookEndpointId={endpoint}"),
            format!("/v2/webhook-endpoints/{endpoint}/test"),
        ),
        (
            "redeliverWebhookDelivery",
            format!("webhookDeliveryId={delivery}"),
            format!("/v2/webhook-deliveries/{delivery}/redeliver"),
        ),
    ] {
        let stub = Stub::new(vec![response(
            "202 Accepted",
            job.clone(),
            &format!("Location: {location}\r\n"),
        )]);
        let mut args = vec![
            "--json",
            "--include-headers",
            "api",
            "call",
            operation,
            "--path",
            &parameter,
            "--idempotency-key",
            "delivery-once",
        ];
        if operation == "redeliverWebhookDelivery" {
            args.extend(["--if-match", "\"current\""]);
        }
        let out = command(&stub.origin, &args, None);
        assert_eq!(success(&out), job);
        assert!(String::from_utf8_lossy(&out.stderr).contains(&format!("location: {location}")));
        let requests = stub.finish();
        assert!(requests[0].starts_with(&format!("POST {path} ")));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("idempotency-key: delivery-once")
        );
        if operation == "redeliverWebhookDelivery" {
            assert!(requests[0].to_ascii_lowercase().contains("if-match: \"current\""));
        }
        assert_eq!(requests[0].split_once("\r\n\r\n").unwrap().1, "");
    }
    let stub = Stub::new(vec![response("200 OK", item.clone(), "ETag: \"delivery\"\r\n")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "getWebhookDelivery",
            "--path",
            &format!("webhookDeliveryId={delivery}"),
        ],
        None,
    );
    assert_eq!(success(&out), item);
    assert!(stub.finish()[0].starts_with(&format!("GET {location} ")));
}

#[test]
fn webhook_request_inspection_preserves_serialized_body_and_nullable_preview() {
    let delivery = "whd_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let event = "evt_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let envelope = json!({"id":event,"type":"ping","api_version":"2026-07-01",
        "created_at":"2026-09-29T10:00:00Z","workspace_id":"space_0197c8f2-5a6b-7cde-8f12-3456789abcde",
        "data":{"message":"escaped \"text\"\nand newline"}})
    .to_string();
    for body in [json!(envelope), Value::Null] {
        let preview = json!({"object_type":"WEBHOOK_DELIVERY_REQUEST","webhook_delivery_id":delivery,
            "url":"https://example.com/hooks","request_headers":{"content-type":"application/json",
            "user-agent":"LingoHub-Webhooks/1","webhook-id":event},"request_body":body,
            "created_at":"2026-09-29T10:00:00.000Z","updated_at":"2026-09-29T10:00:00.000Z"});
        let stub = Stub::new(vec![response("200 OK", preview.clone(), "ETag: \"preview\"\r\n")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                "getWebhookDeliveryRequest",
                "--path",
                &format!("webhookDeliveryId={delivery}"),
            ],
            None,
        );
        assert_eq!(success(&out), preview);
        assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"preview\""));
        let requests = stub.finish();
        assert!(requests[0].starts_with(&format!("GET /v2/webhook-deliveries/{delivery}/request ")));
        assert_eq!(requests[0].split_once("\r\n\r\n").unwrap().1, "");
    }
}

struct UploadFixture(std::path::PathBuf);
impl UploadFixture {
    fn new(name: &str, bytes: &[u8]) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "lh-upload-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        Self(path)
    }
    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
}
impl Drop for UploadFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
    }
}

#[test]
fn multipart_import_preserves_bytes_fields_boundary_and_job_headers() {
    let contents = b"{\"message\":\"Grusse\"}\nlingohub-cli-multipart-0000000000000000";
    let file = UploadFixture::new("en messages.json", contents);
    let job = json!({"object_type":"JOB","job_type":"IMPORT","data":{"id":"imp_123","status":"QUEUED"}});
    let stub = Stub::new(vec![response(
        "202 Accepted",
        job.clone(),
        "Location: /v2/imports/imp_123\r\n",
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "upload",
            "createImport",
            "--path",
            "branchId=branch_123",
            "--upload",
            file.path(),
            "--form",
            "language_code=de",
            "--form",
            "path=src/i18n",
            "--idempotency-key",
            "upload-once",
        ],
        None,
    );
    assert_eq!(success(&out), job);
    assert!(String::from_utf8_lossy(&out.stderr).contains("location: /v2/imports/imp_123"));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert!(request.starts_with("POST /v2/branches/branch_123/imports "));
    assert!(request.contains("idempotency-key: upload-once"));
    assert!(request.contains("boundary=lingohub-cli-multipart-0000000000000001"));
    assert!(request.contains(
        "name=\"file\"; filename=\"en messages.json\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    ));
    assert!(request.contains(std::str::from_utf8(contents).unwrap()));
    assert!(
        request.contains("name=\"language_code\"\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nde\r\n")
    );
    assert!(request.ends_with("\r\n--lingohub-cli-multipart-0000000000000001--\r\n"));
    assert!(!request.contains(file.path()));
}

#[test]
fn multipart_context_image_preserves_plain_boolean_and_unicode_form_fields() {
    let file = UploadFixture::new("checkout.png", b"image fixture");
    let result = json!({"object_type":"CONTEXT_IMAGE", "id":"ci_123"});
    let stub = Stub::new(vec![response("201 Created", result.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "upload",
            "createContextImage",
            "--path",
            "projectId=pr_123",
            "--upload",
            file.path(),
            "--form",
            "language_code=en",
            "--form",
            "auto_connect_suggestions=false",
            "--form",
            "name=日本語",
            "--form",
            "description=Quoted \"description\"\nwith newline",
        ],
        None,
    );
    assert_eq!(success(&out), result);
    let requests = stub.finish();
    assert!(requests[0].starts_with("POST /v2/projects/pr_123/context-images "));
    assert!(requests[0].contains(
        "name=\"auto_connect_suggestions\"\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nfalse\r\n"
    ));
    assert!(requests[0].contains("\r\n\r\n日本語\r\n"));
    assert!(requests[0].contains("Quoted \"description\"\nwith newline"));
}

#[test]
fn context_capture_upload_sends_json_part_and_preserves_replacement_outcomes() {
    let file = UploadFixture::new("screen.png", b"image fixture");
    let rendered_json = r#"[{"keys":[{"key":"checkout.title","table":"app.messages"}],"text":"Pay now","highlight_box":{"x":1.2500,"y":2,"width":30,"height":12}}]"#;
    let rendered_strings = UploadFixture::new("rendered-strings.json", rendered_json.as_bytes());
    let result = json!({"object_type":"CONTEXT_IMAGE","id":"ci_123","capture_key":"checkout/payment-sheet","outcome":"REPLACED","content_hash":"abc","app_version":"2.1.0"});
    let stub = Stub::new(vec![response(
        "200 OK",
        result.clone(),
        "Location: /v2/context-images/ci_123\r\n",
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "upload",
            "createContextImage",
            "--path",
            "projectId=pr_123",
            "--upload",
            file.path(),
            "--form",
            "language_code=de",
            "--form",
            "capture_key=checkout/payment-sheet",
            "--form",
            "app_version=2.1.0",
            "--form-file",
            &format!("rendered_strings={}", rendered_strings.path()),
        ],
        None,
    );
    assert_eq!(success(&out), result);
    let request = &stub.finish()[0];
    assert!(request.contains(&format!(
        "name=\"rendered_strings\"\r\nContent-Type: application/json\r\n\r\n{rendered_json}\r\n"
    )));
    assert!(!request.contains(rendered_strings.path()));
}

#[test]
fn context_capture_empty_rendered_strings_skip_ocr_and_invalid_json_is_local() {
    let file = UploadFixture::new("screen.png", b"image fixture");
    let stub = Stub::new(vec![response(
        "200 OK",
        json!({"object_type":"CONTEXT_IMAGE","id":"ci_123","outcome":"UNCHANGED"}),
        "",
    )]);
    let args = [
        "--json",
        "api",
        "upload",
        "createContextImage",
        "--path",
        "projectId=pr_123",
        "--upload",
        file.path(),
        "--form",
        "language_code=de",
        "--form",
        "rendered_strings=[]",
    ];
    success(&command(&stub.origin, &args, None));
    assert!(stub.finish()[0].contains("Content-Type: application/json\r\n\r\n[]\r\n"));
    for invalid in [
        "rendered_strings=null",
        "rendered_strings={}",
        "rendered_strings=[",
    ] {
        let mut invalid_args = args.to_vec();
        *invalid_args.last_mut().unwrap() = invalid;
        let output = command("http://127.0.0.1:1", &invalid_args, None);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("JSON"));
    }
}

#[test]
fn multipart_denials_fail_locally_before_any_request() {
    let file = UploadFixture::new("source.json", b"{}");
    for extra in [
        vec!["--form", "file=another"],
        vec!["--form", "unknown=value"],
        vec!["--form", "path=a", "--form", "path=b"],
        vec!["--form", "bad\r\nname=x"],
        vec!["--file", "-"],
        vec!["--field", "name=\"value\""],
    ] {
        let mut args = vec![
            "--json",
            "api",
            "upload",
            "createImport",
            "--path",
            "branchId=branch_123",
            "--upload",
            file.path(),
        ];
        args.extend(extra);
        let out = command("http://127.0.0.1:1", &args, None);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    for forms in [
        vec![],
        vec![
            "--form",
            "language_code=en",
            "--form",
            "auto_connect_suggestions=yes",
        ],
    ] {
        let mut args = vec![
            "--json",
            "api",
            "upload",
            "createContextImage",
            "--path",
            "projectId=pr_123",
            "--upload",
            file.path(),
        ];
        args.extend(forms);
        assert_eq!(command("http://127.0.0.1:1", &args, None).status.code(), Some(2));
    }
    for (operation, parameter) in [
        ("getWorkspace", ""),
        ("createImport", "branchId=../other"),
        ("missingOperation", ""),
    ] {
        let mut args = vec!["--json", "api", "upload", operation, "--upload", file.path()];
        if !parameter.is_empty() {
            args.extend(["--path", parameter]);
        }
        assert_eq!(command("http://127.0.0.1:1", &args, None).status.code(), Some(2));
    }
}

#[test]
fn multipart_rejects_empty_missing_oversized_and_unsafe_files() {
    let empty = UploadFixture::new("empty.json", b"");
    // Validate the name before file access; Windows does not permit creating this filename.
    let unsafe_name = empty.0.parent().unwrap().join("bad\"name.json");
    let oversized = UploadFixture::new("large.json", b"{}");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&oversized.0)
        .unwrap()
        .set_len(64 * 1024 * 1024 + 1)
        .unwrap();
    for path in [
        empty.path(),
        unsafe_name.to_str().unwrap(),
        oversized.path(),
        "/does/not/exist/private-file",
        empty.0.parent().unwrap().to_str().unwrap(),
    ] {
        let out = command(
            "http://127.0.0.1:1",
            &[
                "--json",
                "api",
                "upload",
                "createImport",
                "--path",
                "branchId=branch_123",
                "--upload",
                path,
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(2));
        assert!(!String::from_utf8_lossy(&out.stderr).contains(path));
        if path == unsafe_name.to_str().unwrap() {
            assert!(String::from_utf8_lossy(&out.stderr).contains("Upload filename must be"));
        }
    }
}

#[test]
fn multipart_errors_keep_field_details_and_are_not_retried() {
    let file = UploadFixture::new("image.png", b"invalid image");
    let problem = json!({"status":422,"code":"validation_failed","errors":[{"parameter":"file","code":"unsupported_file_extension"}]});
    let stub = Stub::new(vec![response(
        "422 Unprocessable Entity",
        problem.clone(),
        "X-Request-ID: upload-error\r\n",
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "upload",
            "createContextImage",
            "--path",
            "projectId=pr_123",
            "--upload",
            file.path(),
            "--form",
            "language_code=en",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(error["problem"], problem);
    assert_eq!(error["request_id"], "upload-error");
    assert_eq!(stub.finish().len(), 1);
}

fn export_metadata(status: &str, url: Value) -> Value {
    json!({"object_type":"EXPORT","id":"reg_123","status":status,"download_url":url,"download_url_expires_at":"2026-09-29T22:00:00Z"})
}
fn artifact_response(bytes: &[u8]) -> Vec<u8> {
    let mut response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n", bytes.len()).into_bytes();
    response.extend_from_slice(bytes);
    response
}
fn download(origin: &str, output: &std::path::Path, extras: &[&str]) -> Output {
    let mut args = vec![
        "--json",
        "api",
        "download-export",
        "reg_123",
        "--output",
        output.to_str().unwrap(),
    ];
    args.extend_from_slice(extras);
    command(origin, &args, None)
}
fn no_download_leaks(out: &Output) {
    for text in [&out.stdout, &out.stderr] {
        let text = String::from_utf8_lossy(text);
        assert!(!text.contains("secret-link"));
        assert!(!text.contains("provider-private-body"));
    }
}
fn assert_no_staging(fixture: &UploadFixture) {
    assert!(std::fs::read_dir(fixture.0.parent().unwrap()).unwrap().all(|p| {
        !p.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".lingohub-download-")
    }));
}

#[test]
fn export_download_streams_binary_with_separate_unauthenticated_client() {
    let fixture = UploadFixture::new("marker", b"preserve");
    let destination = fixture.0.with_file_name("export.zip");
    let bytes = [0u8, 255, 128, b'P', b'K', b'\n'];
    let artifact = Stub::bytes(vec![artifact_response(&bytes)]);
    let url = format!("{}/artifact.zip?token=secret-link", artifact.origin);
    let api = Stub::new(vec![response(
        "200 OK",
        export_metadata("SUCCEEDED", json!(url)),
        "X-Request-ID: export-req\r\n",
    )]);
    let out = download(&api.origin, &destination, &["--include-headers"]);
    let result = success(&out);
    assert_eq!(result["export_id"], "reg_123");
    assert_eq!(result["bytes_written"], bytes.len());
    assert_eq!(result["output"], destination.to_str().unwrap());
    assert_eq!(std::fs::read(&destination).unwrap(), bytes);
    assert!(String::from_utf8_lossy(&out.stderr).contains("export-req"));
    let api_request = &api.finish()[0];
    assert!(api_request.starts_with("GET /v2/exports/reg_123 "));
    assert!(
        api_request
            .to_lowercase()
            .contains("authorization: bearer test-secret-do-not-print")
    );
    let raw = &artifact.finish()[0];
    assert!(raw.starts_with("GET /artifact.zip?token=secret-link "));
    for header in [
        "authorization:",
        "cookie:",
        "if-match:",
        "idempotency-key:",
        "referer:",
    ] {
        assert!(!raw.to_lowercase().contains(header));
    }
    no_download_leaks(&out);
    assert_no_staging(&fixture);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn export_download_rejects_redirects_partial_and_failed_artifact_responses_without_leaking_urls() {
    let trap = Stub::new(vec![]);
    for status in [
        "302 Found",
        "206 Partial Content",
        "403 Forbidden",
        "500 Server Error",
    ] {
        let fixture = UploadFixture::new("marker", b"preserve");
        let destination = fixture.0.with_file_name("download.zip");
        let artifact = Stub::new(vec![response(
            status,
            json!({"detail":"provider-private-body secret-link"}),
            &format!("Location: {}/?secret-link\r\n", trap.origin),
        )]);
        let api = Stub::new(vec![response(
            "200 OK",
            export_metadata(
                "SUCCEEDED",
                json!(format!("{}/file?secret-link", artifact.origin)),
            ),
            "",
        )]);
        let out = download(&api.origin, &destination, &[]);
        assert_eq!(out.status.code(), Some(1));
        if status.starts_with("403") {
            assert!(String::from_utf8_lossy(&out.stderr).contains("fresh link"));
        }
        assert!(!destination.exists());
        assert_no_staging(&fixture);
        no_download_leaks(&out);
        assert_eq!(api.finish().len(), 1);
        assert_eq!(artifact.finish().len(), 1);
    }
    trap.finish();
}

#[test]
fn export_download_cleans_up_truncated_and_over_limit_streams() {
    for raw in [
        "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 9\r\n\r\nshort",
        "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 12\r\n\r\n0123456789ab",
        "HTTP/1.1 200 OK\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n01234\r\n6\r\n56789a\r\n0\r\n\r\n",
    ] {
        let fixture = UploadFixture::new("marker", b"preserve");
        let destination = fixture.0.with_file_name("download.zip");
        let artifact = Stub::new(vec![raw.into()]);
        let api = Stub::new(vec![response(
            "200 OK",
            export_metadata(
                "SUCCEEDED",
                json!(format!("{}/file?secret-link", artifact.origin)),
            ),
            "",
        )]);
        let out = download(&api.origin, &destination, &["--max-bytes", "10"]);
        assert_eq!(out.status.code(), Some(1));
        assert!(!destination.exists());
        assert_no_staging(&fixture);
        no_download_leaks(&out);
        api.finish();
        artifact.finish();
    }
}

#[test]
fn export_download_validates_resource_state_and_artifact_url_before_fetching() {
    let trap = Stub::new(vec![]);
    let mut cases = vec![
        (
            export_metadata("QUEUED", json!(format!("{}/?secret-link", trap.origin))),
            "Export is not finished; inspect its status before downloading",
        ),
        (
            export_metadata("PROCESSING", Value::Null),
            "Export is not finished; inspect its status before downloading",
        ),
        (
            export_metadata("FAILED", Value::Null),
            "Export did not succeed; inspect its status and entries before downloading",
        ),
        (
            export_metadata("PARTIALLY_SUCCEEDED", Value::Null),
            "Export did not succeed; inspect its status and entries before downloading",
        ),
        (
            export_metadata("SUCCEEDED", Value::Null),
            "Completed export or entry has no download URL",
        ),
        (
            export_metadata("UNKNOWN", Value::Null),
            "Export has an unrecognized status",
        ),
        (
            json!({"object_type":"IMPORT","id":"reg_123","status":"SUCCEEDED","download_url":format!("{}/?secret-link", trap.origin)}),
            "Export metadata does not match the requested export",
        ),
        (
            json!({"object_type":"EXPORT","id":"reg_other","status":"SUCCEEDED","download_url":format!("{}/?secret-link", trap.origin)}),
            "Export metadata does not match the requested export",
        ),
    ];
    for url in [
        "http://example.com/?secret-link",
        "file:///secret-link",
        "https://name:secret-link@example.com/",
        "https://example.com/#secret-link",
        "malformed secret-link",
    ] {
        cases.push((
            export_metadata("SUCCEEDED", json!(url)),
            "Artifact download URL must use HTTPS (or HTTP loopback for local testing), without embedded credentials or a fragment",
        ));
    }
    for (body, expected_error) in cases {
        let fixture = UploadFixture::new("marker", b"preserve");
        let destination = fixture.0.with_file_name("download.zip");
        let api = Stub::new(vec![response("200 OK", body, "")]);
        let out = download(&api.origin, &destination, &[]);
        assert_download_metadata_error(&out, expected_error);
        assert!(!destination.exists());
        assert_no_staging(&fixture);
        no_download_leaks(&out);
        api.finish();
    }
    trap.finish();
}

#[test]
fn export_download_preserves_existing_files_and_rejects_write_flags_before_network() {
    let fixture = UploadFixture::new("existing.zip", b"preserve");
    let out = download("http://127.0.0.1:1", &fixture.0, &[]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(std::fs::read(&fixture.0).unwrap(), b"preserve");
    let destination = fixture.0.with_file_name("new.zip");
    for extra in [
        vec!["--field", "name=1"],
        vec!["--file", "-"],
        vec!["--if-match", "etag"],
        vec!["--idempotency-key", "key"],
        vec!["--max-bytes", "0"],
    ] {
        assert_eq!(
            download("http://127.0.0.1:1", &destination, &extra).status.code(),
            Some(2)
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("missing-target", &destination).unwrap();
        assert_eq!(
            download("http://127.0.0.1:1", &destination, &[]).status.code(),
            Some(2)
        );
        assert!(
            std::fs::symlink_metadata(&destination)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    assert_no_staging(&fixture);
}

#[test]
fn export_download_does_not_replace_a_file_created_during_transfer() {
    let fixture = UploadFixture::new("marker", b"preserve");
    let destination = fixture.0.with_file_name("download.zip");
    let competing = destination.clone();
    let artifact = Stub::with_hook(vec![artifact_response(b"new data")], move |_| {
        std::fs::write(&competing, b"existing data").unwrap();
    });
    let api = Stub::new(vec![response(
        "200 OK",
        export_metadata("SUCCEEDED", json!(format!("{}/file", artifact.origin))),
        "",
    )]);
    let out = download(&api.origin, &destination, &[]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(std::fs::read(&destination).unwrap(), b"existing data");
    assert_no_staging(&fixture);
    api.finish();
    artifact.finish();
}

#[test]
fn export_download_preserves_api_problem_details_and_cleans_staging() {
    let fixture = UploadFixture::new("marker", b"preserve");
    let destination = fixture.0.with_file_name("download.zip");
    let problem = json!({"status":403,"title":"Forbidden","code":"permission_denied"});
    let api = Stub::new(vec![response(
        "403 Forbidden",
        problem.clone(),
        "X-Request-ID: denied\r\n",
    )]);
    let out = download(&api.origin, &destination, &[]);
    assert_eq!(out.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(error["problem"], problem);
    assert_eq!(error["request_id"], "denied");
    assert!(!destination.exists());
    assert_no_staging(&fixture);
    api.finish();
}

fn waiting(origin: &str, operation: &str, parameter: &str, extras: &[&str]) -> Output {
    let mut args = vec!["--json", "api", "wait", operation, "--path", parameter];
    args.extend_from_slice(extras);
    command(origin, &args, None)
}

#[test]
fn polling_follows_pending_states_and_prints_only_the_final_resource() {
    let completed = json!({"object_type":"IMPORT","id":"rin_123","status":"SUCCEEDED","segments_created":2,"status_details":null});
    let api = Stub::new(vec![
        response(
            "200 OK",
            json!({"object_type":"IMPORT","id":"rin_123","status":"QUEUED"}),
            "",
        ),
        response(
            "200 OK",
            json!({"object_type":"IMPORT","id":"rin_123","status":"PROCESSING"}),
            "",
        ),
        response(
            "200 OK",
            completed.clone(),
            "ETag: \"final\"\r\nX-Request-ID: ready\r\n",
        ),
    ]);
    let out = waiting(
        &api.origin,
        "getImport",
        "importId=rin_123",
        &["--interval", "1", "--timeout", "10", "--include-headers"],
    );
    assert_eq!(success(&out), completed);
    assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"final\""));
    let requests = api.finish();
    assert_eq!(requests.len(), 3);
    for raw in requests {
        assert!(raw.starts_with("GET /v2/imports/rin_123 "));
        assert!(
            raw.to_lowercase()
                .contains("authorization: bearer test-secret-do-not-print")
        );
        assert!(!raw.to_lowercase().contains("idempotency-key:"));
    }
}

#[test]
fn polling_supports_each_resource_and_branch_initialization_field() {
    for (operation, parameter, object_type, id, field, path) in [
        (
            "getImport",
            "importId=rin_123",
            "IMPORT",
            "rin_123",
            "status",
            "/v2/imports/rin_123",
        ),
        (
            "getExport",
            "exportId=reg_123",
            "EXPORT",
            "reg_123",
            "status",
            "/v2/exports/reg_123",
        ),
        (
            "getWebhookDelivery",
            "webhookDeliveryId=whd_123",
            "WEBHOOK_DELIVERY",
            "whd_123",
            "status",
            "/v2/webhook-deliveries/whd_123",
        ),
        (
            "getBranch",
            "branchId=prb_123",
            "BRANCH",
            "prb_123",
            "initialization_status",
            "/v2/branches/prb_123",
        ),
        (
            "getProjectMove",
            "projectMoveId=rsy_123",
            "PROJECT_MOVE",
            "rsy_123",
            "status",
            "/v2/project-moves/rsy_123",
        ),
        (
            "getBackupRestore",
            "backupRestoreId=bkr_123",
            "BACKUP_RESTORE",
            "bkr_123",
            "status",
            "/v2/backup-restores/bkr_123",
        ),
        (
            "getBackup",
            "backupId=bkp_123",
            "BACKUP",
            "bkp_123",
            "status",
            "/v2/backups/bkp_123",
        ),
    ] {
        let mut body = json!({"object_type":object_type,"id":id});
        body[field] = json!("SUCCEEDED");
        let api = Stub::new(vec![response("200 OK", body.clone(), "")]);
        assert_eq!(success(&waiting(&api.origin, operation, parameter, &[])), body);
        assert!(api.finish()[0].starts_with(&format!("GET {path} ")));
    }
}

#[test]
fn polling_returns_nonzero_for_terminal_failures_and_preserves_partial_results() {
    for status in ["PARTIALLY_SUCCEEDED", "FAILED", "CANCELLED", "SKIPPED"] {
        let body = json!({"object_type":"EXPORT","id":"reg_123","status":status,"status_details":{"code":"PARTIAL_FAILURE","detail":"Some files failed"}});
        let api = Stub::new(vec![response("200 OK", body.clone(), "")]);
        let out = waiting(&api.origin, "getExport", "exportId=reg_123", &[]);
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        assert!(String::from_utf8_lossy(&out.stderr).contains(status));
        assert_eq!(api.finish().len(), 1);
    }
    let branch = json!({"object_type":"BRANCH","id":"prb_123","status":"SUCCEEDED","initialization_status":"FAILED","initialization_error":"copy failed"});
    let api = Stub::new(vec![response("200 OK", branch.clone(), "")]);
    let out = waiting(&api.origin, "getBranch", "branchId=prb_123", &[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), branch);
    api.finish();
    let delivery = json!({"object_type":"WEBHOOK_DELIVERY","id":"whd_123","status":"FAILED","is_exhausted":false,"next_attempt_at":null,"last_error":"endpoint disabled"});
    let api = Stub::new(vec![response("200 OK", delivery.clone(), "")]);
    let out = waiting(
        &api.origin,
        "getWebhookDelivery",
        "webhookDeliveryId=whd_123",
        &[],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), delivery);
    assert_eq!(api.finish().len(), 1);
}

#[test]
fn polling_rejects_unknown_states_and_mismatched_resources_after_preserving_response() {
    for body in [
        json!({"object_type":"IMPORT","id":"reg_123","status":"SUCCEEDED"}),
        json!({"object_type":"EXPORT","id":"reg_other","status":"SUCCEEDED"}),
        json!({"object_type":"EXPORT","id":"reg_123","status":"FUTURE"}),
        json!({"object_type":"EXPORT","id":"reg_123","status":null}),
        json!({"object_type":"EXPORT","id":"reg_123"}),
    ] {
        let api = Stub::new(vec![response("200 OK", body.clone(), "")]);
        let out = waiting(&api.origin, "getExport", "exportId=reg_123", &[]);
        assert_error_contains(
            &out,
            if body["object_type"] != "EXPORT" || body["id"] != "reg_123" {
                "does not match"
            } else {
                "unrecognized status"
            },
        );
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        assert_eq!(api.finish().len(), 1);
    }
    let api = Stub::new(vec![
        "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".into(),
    ]);
    let out = waiting(&api.origin, "getExport", "exportId=reg_123", &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    api.finish();
}

#[test]
fn polling_deadline_preserves_last_pending_response_and_cancels_a_stalled_read() {
    let pending = json!({"object_type":"EXPORT","id":"reg_123","status":"QUEUED"});
    let api = Stub::new(vec![response("200 OK", pending.clone(), "")]);
    let out = waiting(
        &api.origin,
        "getExport",
        "exportId=reg_123",
        &["--timeout", "1", "--interval", "10"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), pending);
    assert!(String::from_utf8_lossy(&out.stderr).contains("timed out"));
    assert_eq!(api.finish().len(), 1);
    let api = Stub::with_hook(vec![Vec::new()], |_| thread::sleep(Duration::from_secs(2)));
    let out = waiting(&api.origin, "getExport", "exportId=reg_123", &["--timeout", "1"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Waiting timed out"));
    assert_eq!(api.finish().len(), 1);
}

#[test]
fn polling_stops_on_api_error_without_retry_and_preserves_last_observed_state() {
    let pending = json!({"object_type":"EXPORT","id":"reg_123","status":"QUEUED"});
    let problem = json!({"status":429,"title":"Rate limited","code":"rate_limit_exceeded"});
    let api = Stub::new(vec![
        response("200 OK", pending.clone(), ""),
        response("429 Too Many Requests", problem.clone(), "Retry-After: 10\r\n"),
    ]);
    let out = waiting(&api.origin, "getExport", "exportId=reg_123", &["--interval", "1"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), pending);
    let error: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(error["problem"], problem);
    assert_eq!(error["retry_after"], "10");
    assert_eq!(api.finish().len(), 2);
}

#[test]
fn polling_rejects_mutations_bodies_and_invalid_bounds_before_network() {
    for extras in [
        vec!["--file", "-"],
        vec!["--field", "name=1"],
        vec!["--if-match", "etag"],
        vec!["--idempotency-key", "key"],
        vec!["--timeout", "0"],
        vec!["--timeout", "86401"],
        vec!["--interval", "0"],
        vec!["--interval", "61"],
    ] {
        assert_eq!(
            waiting("http://127.0.0.1:1", "getExport", "exportId=reg_123", &extras)
                .status
                .code(),
            Some(2)
        );
    }
    assert_eq!(
        waiting("http://127.0.0.1:1", "createExport", "branchId=prb_123", &[])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        waiting("http://127.0.0.1:1", "getExport", "exportId=../other", &[])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        waiting("http://127.0.0.1:1", "getExport", "branchId=prb_123", &[])
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn workspace_role_discovery_preserves_public_metadata_filters_and_cursors() {
    let id = "rol_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    for role_type in ["OWNER", "CUSTOM"] {
        let role = json!({"object_type":"WORKSPACE_ROLE","id":id,"name":"Translator","description":null,"type":role_type,"created_at":"2026-09-29T10:00:00.123Z","updated_at":"2026-09-29T10:00:00.123Z"});
        let stub = Stub::new(vec![response(
            "200 OK",
            role.clone(),
            "ETag: \"role-version\"\r\n",
        )]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                "getWorkspaceRole",
                "--path",
                &format!("workspaceRoleId={id}"),
            ],
            None,
        );
        assert_eq!(success(&out), role);
        assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"role-version\""));
        assert!(stub.finish()[0].starts_with(&format!("GET /v2/workspace-roles/{id} ")));
        let page = page(json!([role]), Some("--next+/="));
        let stub = Stub::new(vec![response("200 OK", page.clone(), "")]);
        let filter = json!({"where":{"_and":[{"workspace_role[type]":{"_eq":role_type}}]}}).to_string();
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "listWorkspaceRoles",
                "--query",
                "limit=1",
                "--query",
                "cursor=--previous+/=",
                "--query",
                &format!("filter={filter}"),
            ],
            None,
        );
        assert_eq!(success(&out), page);
        let requests = stub.finish();
        let path = requests[0].split_whitespace().nth(1).unwrap();
        let url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
        assert_eq!(url.path(), "/v2/workspace-roles");
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "filter" && value == filter)
        );
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "cursor" && value == "--previous+/=")
        );
    }
}

#[test]
fn workspace_role_discovery_preserves_permission_and_visibility_errors() {
    for (status, code) in [
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let problem = json!({"status":if code=="insufficient_permissions" {403} else {404},"title":"Role unavailable","code":code,"request_id":"role-request"});
        let stub = Stub::new(vec![response(status, problem.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "getWorkspaceRole",
                "--path",
                "workspaceRoleId=rol_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains(code));
        assert!(out.stdout.is_empty());
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn member_discovery_preserves_identity_nullable_profiles_filters_and_cursor_values() {
    let id = "wsu_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let member = json!({"object_type":"MEMBER","id":id,"user_id":"usr_0197c8f2-5a6b-7cde-8f12-3456789abcde","workspace_role_id":"rol_0197c8f2-5a6b-7cde-8f12-3456789abcde","email":null,"first_name":"Älex 😀","last_name":null,"is_support":false,"created_at":"2026-01-01T10:10:00.000Z","updated_at":"2026-01-01T10:10:00.000Z"});
    let stub = Stub::new(vec![response(
        "200 OK",
        member.clone(),
        "ETag: \"member-version\"\r\n",
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "getMember",
            "--path",
            &format!("memberId={id}"),
        ],
        None,
    );
    assert_eq!(success(&out), member);
    assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"member-version\""));
    assert!(stub.finish()[0].starts_with(&format!("GET /v2/members/{id} ")));
    let page = page(json!([member]), Some("--next+/="));
    let stub = Stub::new(vec![response("200 OK", page.clone(), "")]);
    let filter = json!({"where":{"_and":[{"member[first_name]":{"_eq":"Älex 😀"}}]}}).to_string();
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listMembers",
            "--query",
            "limit=1",
            "--query",
            "cursor=--previous+/=",
            "--query",
            &format!("filter={filter}"),
        ],
        None,
    );
    assert_eq!(success(&out), page);
    let requests = stub.finish();
    let path = requests[0].split_whitespace().nth(1).unwrap();
    let url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
    assert_eq!(url.path(), "/v2/members");
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "filter" && value == filter)
    );
    assert!(
        url.query_pairs()
            .any(|(key, value)| key == "cursor" && value == "--previous+/=")
    );
}

#[test]
fn member_discovery_preserves_current_permission_and_visibility_errors() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let problem = json!({"title":"Member unavailable","code":code,"request_id":"member-request"});
        let stub = Stub::new(vec![response(status, problem, "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "getMember",
                "--path",
                "memberId=wsu_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert!(String::from_utf8_lossy(&out.stderr).contains(code));
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn member_removal_uses_membership_identity_validator_and_empty_success() {
    let id = "wsu_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let stub = Stub::new(vec![
        "HTTP/1.1 204 No Content\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".to_string(),
    ]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "deleteMember",
            "--path",
            &format!("memberId={id}"),
            "--if-match",
            "\"member-current\"",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty());
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!("DELETE /v2/members/{id} ")));
    assert!(
        requests[0]
            .to_lowercase()
            .contains("if-match: \"member-current\"")
    );
}

#[test]
fn member_removal_preserves_owner_stale_visibility_and_permission_errors_without_retry() {
    for (status, code) in [
        ("400 Bad Request", "invalid_parameter"),
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("409 Conflict", "owner_membership_protected"),
        ("412 Precondition Failed", "precondition_failed"),
    ] {
        let problem =
            json!({"title":"Membership removal rejected","code":code,"request_id":"remove-member-request"});
        let stub = Stub::new(vec![response(status, problem, "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "deleteMember",
                "--path",
                "memberId=wsu_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(code));
        assert!(stderr.contains("remove-member-request"));
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn stack_role_discovery_preserves_public_metadata_filters_and_cursors() {
    let id = "stackr_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    for role_type in ["OWNER", "CUSTOM"] {
        let role = json!({"object_type":"STACK_ROLE","id":id,"name":"Translator","description":null,"type":role_type,"created_at":"2026-09-29T10:00:00.123Z","updated_at":"2026-09-29T10:00:00.123Z"});
        let stub = Stub::new(vec![response(
            "200 OK",
            role.clone(),
            "ETag: \"role-version\"\r\n",
        )]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                "getStackRole",
                "--path",
                &format!("stackRoleId={id}"),
            ],
            None,
        );
        assert_eq!(success(&out), role);
        assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"role-version\""));
        assert!(stub.finish()[0].starts_with(&format!("GET /v2/stack-roles/{id} ")));
        let page = page(json!([role]), Some("--next+/="));
        let stub = Stub::new(vec![response("200 OK", page.clone(), "")]);
        let filter = json!({"where":{"_and":[{"stack_role[type]":{"_eq":role_type}}]}}).to_string();
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "listStackRoles",
                "--query",
                "limit=1",
                "--query",
                "cursor=--previous+/=",
                "--query",
                &format!("filter={filter}"),
            ],
            None,
        );
        assert_eq!(success(&out), page);
        let requests = stub.finish();
        let path = requests[0].split_whitespace().nth(1).unwrap();
        let url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
        assert_eq!(url.path(), "/v2/stack-roles");
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "filter" && value == filter)
        );
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "cursor" && value == "--previous+/=")
        );
    }
}

#[test]
fn stack_role_discovery_preserves_permission_and_visibility_errors() {
    for (status, code) in [
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let problem = json!({"status":if code=="insufficient_permissions" {403} else {404},"title":"Role unavailable","code":code,"request_id":"role-request"});
        let stub = Stub::new(vec![response(status, problem.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "getStackRole",
                "--path",
                "stackRoleId=stackr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains(code));
        assert!(out.stdout.is_empty());
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn invitation_discovery_preserves_states_metadata_filters_and_cursors() {
    let id = "svi_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    for state in ["PENDING", "ACCEPTED", "DECLINED", "EXPIRED"] {
        let accepted = if state == "ACCEPTED" {
            Some("2026-09-29T10:01:00.123Z")
        } else {
            None
        };
        let declined = if state == "DECLINED" {
            Some("2026-09-29T10:02:00.123Z")
        } else {
            None
        };
        let invitation = json!({"object_type":"INVITATION","id":id,"workspace_role_id":"rol_0197c8f2-5a6b-7cde-8f12-3456789abcde","inviter_actor_id":"act_0197c8f2-5a6b-7cde-8f12-3456789abcde","invitee_user_id":null,"email":"invitee@example.test","invitee_name":"Älex 😀","message":null,"is_stack_assignment_configured":false,"status":state,"expires_at":"2026-10-01T10:00:00.123Z","accepted_at":accepted,"declined_at":declined,"created_at":"2026-09-29T10:00:00.123Z","updated_at":"2026-09-29T10:00:00.123Z"});
        let stub = Stub::new(vec![response(
            "200 OK",
            invitation.clone(),
            "ETag: \"invitation-version\"\r\n",
        )]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                "getInvitation",
                "--path",
                &format!("invitationId={id}"),
            ],
            None,
        );
        assert_eq!(success(&out), invitation);
        assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"invitation-version\""));
        assert!(stub.finish()[0].starts_with(&format!("GET /v2/invitations/{id} ")));
        let result = page(json!([invitation]), Some("--next+/="));
        let stub = Stub::new(vec![response("200 OK", result.clone(), "")]);
        let filter =
            json!({"where":{"_and":[{"invitation[email]":{"_eq":"invitee@example.test"}}]}}).to_string();
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "listInvitations",
                "--query",
                "limit=1",
                "--query",
                "cursor=--previous+/=",
                "--query",
                &format!("filter={filter}"),
            ],
            None,
        );
        assert_eq!(success(&out), result);
        let requests = stub.finish();
        let path = requests[0].split_whitespace().nth(1).unwrap();
        let url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
        assert_eq!(url.path(), "/v2/invitations");
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "filter" && value == filter)
        );
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "cursor" && value == "--previous+/=")
        );
    }
}

#[test]
fn invitation_discovery_preserves_token_permission_visibility_and_filter_errors() {
    for (status, number, code) in [
        ("401 Unauthorized", 401, "revoked_api_key"),
        ("403 Forbidden", 403, "insufficient_permissions"),
        ("404 Not Found", 404, "resource_not_found"),
        ("400 Bad Request", 400, "invalid_filter"),
    ] {
        let problem = json!({"status":number,"title":"Invitation unavailable","code":code,"request_id":"invitation-request"});
        let stub = Stub::new(vec![response(status, problem, "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "getInvitation",
                "--path",
                "invitationId=svi_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(error.contains(code));
        assert!(error.contains("invitation-request"));
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn invitation_removal_uses_invitation_identity_validator_and_empty_success() {
    let id = "svi_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let stub = Stub::new(vec![
        "HTTP/1.1 204 No Content\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".to_string(),
    ]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "deleteInvitation",
            "--path",
            &format!("invitationId={id}"),
            "--if-match",
            "\"invitation-current\"",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty());
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!("DELETE /v2/invitations/{id} ")));
    assert!(
        requests[0]
            .to_lowercase()
            .contains("if-match: \"invitation-current\"")
    );
}

#[test]
fn invitation_removal_preserves_stale_visibility_and_permission_errors_without_retry() {
    for (status, code) in [
        ("400 Bad Request", "invalid_parameter"),
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("412 Precondition Failed", "precondition_failed"),
    ] {
        let problem = json!({"title":"Invitationship removal rejected","code":code,"request_id":"remove-invitation-request"});
        let stub = Stub::new(vec![response(status, problem, "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "deleteInvitation",
                "--path",
                "invitationId=svi_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(code));
        assert!(stderr.contains("remove-invitation-request"));
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn invitation_assignment_removal_uses_invitation_identity_validator_and_empty_success() {
    let id = "svi_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let stub = Stub::new(vec![
        "HTTP/1.1 204 No Content\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".to_string(),
    ]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "deleteInvitationStackAssignment",
            "--path",
            &format!("invitationId={id}"),
            "--if-match",
            "\"invitation-current\"",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty());
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!("DELETE /v2/invitations/{id}/stack-assignment ")));
    assert!(
        requests[0]
            .to_lowercase()
            .contains("if-match: \"invitation-current\"")
    );
}

#[test]
fn invitation_assignment_removal_preserves_stale_visibility_and_permission_errors_without_retry() {
    for (status, code) in [
        ("400 Bad Request", "invalid_parameter"),
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("412 Precondition Failed", "precondition_failed"),
        ("409 Conflict", "conflict"),
        ("409 Conflict", "owner_membership_protected"),
    ] {
        let problem = json!({"title":"Invitation assignment removal rejected","code":code,"request_id":"remove-invitation-request"});
        let stub = Stub::new(vec![response(status, problem, "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "deleteInvitationStackAssignment",
                "--path",
                "invitationId=svi_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(code));
        assert!(stderr.contains("remove-invitation-request"));
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn stack_member_discovery_preserves_composite_identity_language_map_and_paging() {
    let stack = "stack_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let member = "wsu_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    for permissions in [
        json!({}),
        json!({"de":"READ_ACCESS","en":"RESTRICTED_ACCESS","fr":"FULL_ACCESS"}),
    ] {
        let assignment = json!({"object_type":"STACK_MEMBER","id":member,"stack_id":stack,"user_id":"usr_0197c8f2-5a6b-7cde-8f12-3456789abcde","stack_role_id":"stackr_0197c8f2-5a6b-7cde-8f12-3456789abcde","email":null,"first_name":"Älex 😀","last_name":null,"language_permissions":permissions,"created_at":"2026-09-30T00:00:00.123Z","updated_at":"2026-09-30T00:00:00.123Z"});
        let stub = Stub::new(vec![response(
            "200 OK",
            assignment.clone(),
            "ETag: \"assignment-version\"\r\n",
        )]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                "getStackMember",
                "--path",
                &format!("stackId={stack}"),
                "--path",
                &format!("memberId={member}"),
            ],
            None,
        );
        assert_eq!(success(&out), assignment);
        assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"assignment-version\""));
        assert!(stub.finish()[0].starts_with(&format!("GET /v2/stacks/{stack}/members/{member} ")));
        let result = page(json!([assignment]), Some("--next+/="));
        let stub = Stub::new(vec![response("200 OK", result.clone(), "")]);
        let filter = json!({"where":{"_and":[{"stack_member[id]":{"_eq":member}}]}}).to_string();
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "listStackMembers",
                "--path",
                &format!("stackId={stack}"),
                "--query",
                "limit=1",
                "--query",
                "cursor=--previous+/=",
                "--query",
                &format!("filter={filter}"),
            ],
            None,
        );
        assert_eq!(success(&out), result);
        let requests = stub.finish();
        let path = requests[0].split_whitespace().nth(1).unwrap();
        let url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
        assert_eq!(url.path(), format!("/v2/stacks/{stack}/members"));
        assert!(url.query_pairs().any(|(k, v)| k == "filter" && v == filter));
        assert!(
            url.query_pairs()
                .any(|(k, v)| k == "cursor" && v == "--previous+/=")
        );
    }
}

#[test]
fn stack_member_discovery_preserves_token_permission_visibility_and_cursor_errors() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("400 Bad Request", "invalid_cursor"),
    ] {
        let problem =
            json!({"title":"Stack member unavailable","code":code,"request_id":"stack-member-request"});
        let stub = Stub::new(vec![response(status, problem, "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "listStackMembers",
                "--path",
                "stackId=stack_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(code));
        assert!(stderr.contains("stack-member-request"));
        assert!(out.stdout.is_empty());
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn stack_member_removal_uses_composite_identity_validator_and_empty_success() {
    let id = "wsu_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let stub = Stub::new(vec![
        "HTTP/1.1 204 No Content\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".to_string(),
    ]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "deleteStackMember",
            "--path",
            "stackId=stack_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            "--path",
            &format!("memberId={id}"),
            "--if-match",
            "\"stack-member-current\"",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty());
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!(
        "DELETE /v2/stacks/stack_0197c8f2-5a6b-7cde-8f12-3456789abcde/members/{id} "
    )));
    assert!(
        requests[0]
            .to_lowercase()
            .contains("if-match: \"stack-member-current\"")
    );
}

#[test]
fn stack_member_removal_preserves_stale_visibility_and_permission_errors_without_retry() {
    for (status, code) in [
        ("400 Bad Request", "invalid_parameter"),
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("412 Precondition Failed", "precondition_failed"),
        ("409 Conflict", "owner_membership_protected"),
    ] {
        let problem = json!({"title":"Stack membership removal rejected","code":code,"request_id":"remove-stack-member-request"});
        let stub = Stub::new(vec![response(status, problem, "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "deleteStackMember",
                "--path",
                "stackId=stack_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--path",
                "memberId=wsu_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(code));
        assert!(stderr.contains("remove-stack-member-request"));
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn stack_member_set_preserves_complete_map_role_composite_path_and_validator() {
    let stack = "stack_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let member = "wsu_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let role = "stackr_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    for (status, map) in [
        ("201 Created", json!({"de":"READ_ACCESS"})),
        ("200 OK", json!({})),
    ] {
        let body = json!({"stack_role_id":role,"language_permissions":map});
        let result = json!({"object_type":"STACK_MEMBER","id":member,"stack_id":stack,"stack_role_id":role,"language_permissions":map});
        let location = format!("/v2/stacks/{stack}/members/{member}");
        let stub = Stub::new(vec![response(
            status,
            result.clone(),
            &format!("Location: {location}\r\n"),
        )]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                "setStackMember",
                "--path",
                &format!("stackId={stack}"),
                "--path",
                &format!("memberId={member}"),
                "--if-match",
                "\"current\"",
                "--file",
                "-",
            ],
            Some(&body.to_string()),
        );
        assert_eq!(success(&out), result);
        assert!(String::from_utf8_lossy(&out.stderr).contains(&format!("location: {location}")));
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("PUT {location} ")));
        assert!(requests[0].to_lowercase().contains("if-match: \"current\""));
        let sent: Value = serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(sent, body);
    }
}

#[test]
fn stack_member_set_retains_field_errors_and_never_retries_mutations() {
    for (status, code) in [
        ("409 Conflict", "owner_membership_protected"),
        ("412 Precondition Failed", "precondition_failed"),
        ("422 Unprocessable Entity", "validation_failed"),
        ("403 Forbidden", "insufficient_permissions"),
    ] {
        let body = json!({"title":"Assignment rejected", "code":code, "request_id":"set-stack-member-request",
            "errors":[{"pointer":"#/stack_role_id","code":"not_found","detail":"Role unavailable"}]});
        let stub = Stub::new(vec![response(status, body, "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "setStackMember",
                "--path",
                "stackId=stack_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--path",
                "memberId=wsu_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--file",
                "-",
            ],
            Some("{}"),
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(code));
        assert!(stderr.contains("#/stack_role_id"));
        assert!(stderr.contains("set-stack-member-request"));
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn secret_rotation_prints_original_material_only_on_stdout_with_condition_and_key() {
    let endpoint = "whe_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let body = json!({"object_type":"WEBHOOK_ENDPOINT_SECRET", "id":endpoint,
        "secret":"whsec_explicit-once", "created_at":"2026-09-30T00:00:00.123Z", "updated_at":"2026-09-30T01:00:00.123Z"});
    let stub = Stub::new(vec![response(
        "200 OK",
        body.clone(),
        "Cache-Control: no-store\r\n",
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "rotateWebhookEndpointSecret",
            "--path",
            &format!("webhookEndpointId={endpoint}"),
            "--if-match",
            "\"current\"",
            "--idempotency-key",
            "explicit-rotation",
        ],
        None,
    );
    assert_eq!(success(&out), body);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cache-control: no-store"));
    assert!(!stderr.contains("whsec_explicit-once"));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!("POST /v2/webhook-endpoints/{endpoint}/rotate-secret ")));
    assert!(requests[0].to_lowercase().contains("if-match: \"current\""));
    assert!(
        requests[0]
            .to_lowercase()
            .contains("idempotency-key: explicit-rotation")
    );
    assert!(requests[0].split_once("\r\n\r\n").unwrap().1.is_empty());
}

#[test]
fn secret_rotation_never_retries_unknown_stale_or_permission_results() {
    for (status, code) in [
        ("409 Conflict", "idempotency_outcome_unknown"),
        ("409 Conflict", "idempotency_key_in_use"),
        ("412 Precondition Failed", "precondition_failed"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let stub = Stub::new(vec![response(
            status,
            json!({"title":"Rotation rejected", "code":code, "request_id":"secret-rotation-request"}),
            "",
        )]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "rotateWebhookEndpointSecret",
                "--path",
                "webhookEndpointId=whe_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--idempotency-key",
                "one-time",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(code));
        assert!(stderr.contains("secret-rotation-request"));
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn git_discovery_preserves_configuration_binding_and_validators() {
    for (operation, parameter, id, resource, body) in [
        (
            "getProjectGitIntegration",
            "projectId",
            "pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            "projects",
            json!({"object_type":"PROJECT_GIT_INTEGRATION","connection_status":"SUSPENDED","provider_origin":"https://git.example.com:8443","import_settings":{"sync_mode":"NONE","schedule":null},"export_settings":{"sync_mode":"ON_COMPLETION"},"resource_filters":[]}),
        ),
        (
            "getProjectGitIntegration",
            "projectId",
            "pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            "projects",
            json!({"object_type":"PROJECT_GIT_INTEGRATION","provider":"AWS_CODE_COMMIT","provider_origin":null,"provider_region":"us-east-1"}),
        ),
        (
            "getBranchGitIntegration",
            "branchId",
            "prb_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            "branches",
            json!({"object_type":"BRANCH_GIT_INTEGRATION","git_branch":"remote/release","last_push_commit_sha":"commit","last_pulled_commit_sha":null,"is_protected":true}),
        ),
    ] {
        let stub = Stub::new(vec![response("200 OK", body.clone(), "ETag: \"git-state\"\r\n")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                operation,
                "--path",
                &format!("{parameter}={id}"),
            ],
            None,
        );
        assert_eq!(success(&out), body);
        assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"git-state\""));
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("GET /v2/{resource}/{id}/integrations/git ")));
    }
}

#[test]
fn git_discovery_preserves_visibility_permission_and_parameter_errors() {
    for operation in ["getProjectGitIntegration", "getBranchGitIntegration"] {
        for (status, code) in [
            ("404 Not Found", "resource_not_found"),
            ("403 Forbidden", "insufficient_permissions"),
            ("400 Bad Request", "unknown_parameter"),
        ] {
            let stub = Stub::new(vec![response(
                status,
                json!({"title":"Git configuration unavailable","code":code,"request_id":"git-discovery-request"}),
                "",
            )]);
            let path = if operation == "getProjectGitIntegration" {
                "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde"
            } else {
                "branchId=prb_0197c8f2-5a6b-7cde-8f12-3456789abcde"
            };
            let out = command(
                &stub.origin,
                &["--json", "api", "call", operation, "--path", path],
                None,
            );
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(stderr.contains(code));
            assert!(stderr.contains("git-discovery-request"));
            assert_eq!(stub.finish().len(), 1);
        }
    }
}

#[test]
fn git_push_job_and_export_list_preserve_stored_results() {
    let pending = json!({"object_type":"JOB","job_type":"EXPORT","data":{"object_type":"EXPORT","id":"reg_123","status":"QUEUED","git_result":null}});
    let stub = Stub::new(vec![response(
        "202 Accepted",
        pending.clone(),
        "Location: /v2/exports/reg_123\r\n",
    )]);
    let out = command(
        &stub.origin,
        &["--json", "api", "call", "pushToGit", "--path", "branchId=prb_123"],
        Some("{}"),
    );
    assert_eq!(success(&out), pending);
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("POST /v2/branches/prb_123/integrations/git/push "));
    let listed = json!({"object_type":"LIST","has_more":false,"next_cursor":null,"data":[{"object_type":"EXPORT","id":"reg_123","status":"SUCCEEDED","git_result":{"commit_sha":"confirmed","git_branch":"translations/main"}},{"object_type":"EXPORT","id":"reg_124","status":"SKIPPED","git_result":null}]});
    let stub = Stub::new(vec![response("200 OK", listed.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listExports",
            "--path",
            "branchId=prb_123",
        ],
        None,
    );
    assert_eq!(success(&out), listed);
    assert_eq!(stub.finish().len(), 1);
}

#[test]
fn git_export_wait_preserves_confirmed_or_absent_results_and_terminal_errors() {
    for (status, result, exit) in [
        (
            "SUCCEEDED",
            json!({"commit_sha":"provider-hash","git_branch":"translations/Grüße"}),
            0,
        ),
        ("SKIPPED", Value::Null, 1),
        ("FAILED", Value::Null, 1),
        ("SUCCEEDED", Value::Null, 0),
    ] {
        let body = json!({"object_type":"EXPORT","id":"reg_123","status":status,"git_result":result});
        let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "wait",
                "getExport",
                "--path",
                "exportId=reg_123",
                "--interval",
                "1",
            ],
            None,
        );
        assert_eq!(
            out.status.code(),
            Some(exit),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn git_pull_source_preserves_observations_and_absence_for_every_terminal_outcome() {
    let source = json!({"observed_head_commit_sha":"observed-head","git_branch":"remote-main"});
    for (status, observed, exit) in [
        ("SUCCEEDED", source.clone(), 0),
        ("PARTIALLY_SUCCEEDED", source.clone(), 1),
        ("FAILED", source, 1),
        ("SUCCEEDED", serde_json::Value::Null, 0),
    ] {
        let pending = json!({"object_type":"IMPORT","id":"rin_123","status":"PROCESSING","git_source":null});
        let completed = json!({"object_type":"IMPORT","id":"rin_123","status":status,"git_source":observed});
        let stub = Stub::new(vec![
            response("200 OK", pending, ""),
            response("200 OK", completed.clone(), ""),
        ]);
        let output = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "wait",
                "getImport",
                "--path",
                "importId=rin_123",
                "--interval",
                "1",
            ],
            None,
        );
        assert_eq!(output.status.code(), Some(exit));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
            completed
        );
        let requests = stub.finish();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| request.starts_with("GET /v2/imports/rin_123 "))
        );
    }
}

#[test]
fn git_pull_request_outcomes_keep_links_and_explicit_absence_when_waiting() {
    for result in [
        json!({"commit_sha":"confirmed-new","git_branch":"translations/main","pull_request_reference":"42","pull_request_url":"https://github.com/example/repo/pull/42"}),
        json!({"commit_sha":"confirmed-reuse","git_branch":"translations/main","pull_request_reference":"42","pull_request_url":null}),
        json!({"commit_sha":"confirmed-direct","git_branch":"main","pull_request_reference":null,"pull_request_url":null}),
    ] {
        let pending = json!({"object_type":"EXPORT","id":"reg_123","status":"PROCESSING","git_result":null});
        let completed =
            json!({"object_type":"EXPORT","id":"reg_123","status":"SUCCEEDED","git_result":result});
        let stub = Stub::new(vec![
            response("200 OK", pending, ""),
            response("200 OK", completed.clone(), ""),
        ]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "wait",
                "getExport",
                "--path",
                "exportId=reg_123",
                "--interval",
                "1",
            ],
            None,
        );
        assert_eq!(success(&out), completed);
        let requests = stub.finish();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| request.starts_with("GET /v2/exports/reg_123 "))
        );
    }
}

#[test]
fn backup_discovery_preserves_retained_metadata_and_cursors() {
    let id = "bkp_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let project = "pr_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let body = json!({"object_type":"BACKUP","id":id,"project_id":project,"branch_id":null,"name":"Before release","branch_name":"historical-main","status":"SUCCEEDED","source_language_code":"en","target_language_codes":["de"],"created_at":"2026-09-30T00:00:00.000Z","updated_at":"2026-09-30T00:01:00.000Z"});
    let page =
        json!({"object_type":"LIST","data":[body.clone()],"has_more":true,"next_cursor":"opaque-backup"});
    let stub = Stub::new(vec![response("200 OK", page.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listBackups",
            "--path",
            &format!("projectId={project}"),
            "--query",
            "limit=1",
            "--query",
            "cursor=previous-page",
        ],
        None,
    );
    assert_eq!(success(&out), page);
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!("GET /v2/projects/{project}/backups?")));
    assert!(requests[0].contains("limit=1"));
    assert!(requests[0].contains("cursor=previous-page"));
    let stub = Stub::new(vec![response(
        "200 OK",
        body.clone(),
        "ETag: \"backup-state\"\r\n",
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "getBackup",
            "--path",
            &format!("backupId={id}"),
        ],
        None,
    );
    assert_eq!(success(&out), body);
    assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"backup-state\""));
    assert!(stub.finish()[0].starts_with(&format!("GET /v2/backups/{id} ")));
}

#[test]
fn backup_discovery_preserves_scope_permission_and_cursor_errors() {
    for (operation, path) in [
        ("getBackup", "backupId=bkp_0197c8f2-5a6b-7cde-8f12-3456789abcde"),
        ("listBackups", "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde"),
    ] {
        for (status, code) in [
            ("404 Not Found", "resource_not_found"),
            ("403 Forbidden", "insufficient_permissions"),
            ("400 Bad Request", "invalid_cursor"),
        ] {
            let stub = Stub::new(vec![response(
                status,
                json!({"title":"Backup unavailable","code":code,"request_id":"backup-discovery-request"}),
                "",
            )]);
            let out = command(
                &stub.origin,
                &["--json", "api", "call", operation, "--path", path],
                None,
            );
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(stderr.contains(code));
            assert!(stderr.contains("backup-discovery-request"));
            assert_eq!(stub.finish().len(), 1);
        }
    }
}

#[test]
fn backup_creation_preserves_required_name_job_and_polling_location() {
    let id = "bkp_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let branch = "prb_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let body = json!({"object_type":"JOB","job_type":"BACKUP","data":{"object_type":"BACKUP","id":id,"name":"Before release — Grüße","status":"QUEUED"}});
    let stub = Stub::new(vec![response(
        "202 Accepted",
        body.clone(),
        &format!("Location: /v2/backups/{id}\r\n"),
    )]);
    let input = "{\"name\":\"Before release — Grüße\"}";
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "--idempotency-key",
            "snapshot-once",
            "api",
            "call",
            "createBackup",
            "--path",
            &format!("branchId={branch}"),
            "--file",
            "-",
        ],
        Some(input),
    );
    assert_eq!(success(&out), body);
    assert!(String::from_utf8_lossy(&out.stderr).contains(&format!("location: /v2/backups/{id}")));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!("POST /v2/branches/{branch}/backups ")));
    assert!(requests[0].contains(input));
    assert!(
        requests[0]
            .to_lowercase()
            .contains("idempotency-key: snapshot-once")
    );
}

#[test]
fn backup_wait_preserves_terminal_metadata_without_rescheduling() {
    for status in [
        "SUCCEEDED",
        "PARTIALLY_SUCCEEDED",
        "FAILED",
        "CANCELLED",
        "SKIPPED",
    ] {
        let pending = json!({"object_type":"BACKUP","id":"bkp_123","status":"PROCESSING","name":"Snapshot"});
        let body =
            json!({"object_type":"BACKUP","id":"bkp_123","status":status,"name":"Snapshot","branch_id":null});
        let stub = Stub::new(vec![
            response("200 OK", pending, ""),
            response("200 OK", body.clone(), ""),
        ]);
        let out = waiting(
            &stub.origin,
            "getBackup",
            "backupId=bkp_123",
            &["--interval", "1", "--timeout", "5"],
        );
        assert_eq!(out.status.code(), Some(if status == "SUCCEEDED" { 0 } else { 1 }));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap(),
            body
        );
        let requests = stub.finish();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|r| r.starts_with("GET /v2/backups/bkp_123 ")));
    }
}

#[test]
fn restore_discovery_preserves_public_references_languages_and_cursor_arguments() {
    let body = json!({"object_type":"LIST","has_more":false,"next_cursor":null,"data":[{"object_type":"BACKUP_RESTORE","id":"bkr_123","branch_id":"prb_123","backup_id":"bkp_123","project_id":"pr_123","status":"QUEUED","language_codes":["en","de"],"created_at":"2026-09-30T00:00:00.000Z","updated_at":"2026-09-30T00:00:00.000Z"}]});
    let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listBackupRestores",
            "--path",
            "branchId=prb_123",
            "--query",
            "limit=1",
            "--query",
            "cursor=opaque",
        ],
        None,
    );
    assert_eq!(success(&out), body);
    let requests = stub.finish();
    assert!(requests[0].starts_with("GET /v2/branches/prb_123/backup-restores?"));
    assert!(requests[0].contains("limit=1"));
    assert!(requests[0].contains("cursor=opaque"));
}

#[test]
fn restore_wait_preserves_terminal_metadata_without_changing_content() {
    for status in [
        "SUCCEEDED",
        "PARTIALLY_SUCCEEDED",
        "FAILED",
        "CANCELLED",
        "SKIPPED",
    ] {
        let pending = json!({"object_type":"BACKUP_RESTORE","id":"bkr_123","status":"PROCESSING"});
        let body = json!({"object_type":"BACKUP_RESTORE","id":"bkr_123","status":status,"backup_id":"bkp_123","language_codes":["en"]});
        let stub = Stub::new(vec![
            response("200 OK", pending, ""),
            response("200 OK", body.clone(), ""),
        ]);
        let out = waiting(
            &stub.origin,
            "getBackupRestore",
            "backupRestoreId=bkr_123",
            &["--interval", "1", "--timeout", "5"],
        );
        assert_eq!(out.status.code(), Some(if status == "SUCCEEDED" { 0 } else { 1 }));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap(),
            body
        );
        let requests = stub.finish();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|r| r.starts_with("GET /v2/backup-restores/bkr_123 "))
        );
    }
}

#[test]
fn background_failure_details_are_preserved_without_resubmitting_work() {
    for (operation, parameter, object, id, path) in [
        (
            "getBackup",
            "backupId=bkp_123",
            "BACKUP",
            "bkp_123",
            "/v2/backups/bkp_123",
        ),
        (
            "getBackupRestore",
            "backupRestoreId=bkr_123",
            "BACKUP_RESTORE",
            "bkr_123",
            "/v2/backup-restores/bkr_123",
        ),
    ] {
        for code in [
            "BACKUP_FAILED",
            "RESTORE_FAILED",
            "EXECUTION_FAILED",
            "UNKNOWN_ERROR",
            "FUTURE_FAILURE",
        ] {
            let body = json!({"object_type":object,"id":id,"status":"FAILED","status_details":{"code":code,"detail":"Inspect completed work before submitting again."}});
            let stub = Stub::new(vec![response(
                "200 OK",
                body.clone(),
                "ETag: \"failure-tag\"\r\n",
            )]);
            let out = waiting(&stub.origin, operation, parameter, &["--include-headers"]);
            assert_eq!(out.status.code(), Some(1));
            assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
            let requests = stub.finish();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].starts_with(&format!("GET {path} ")));
        }
    }
}

#[test]
fn restore_wait_timeout_preserves_last_pending_state() {
    let pending =
        json!({"object_type":"BACKUP_RESTORE","id":"bkr_123","status":"QUEUED","language_codes":["en"]});
    let stub = Stub::new(vec![response("200 OK", pending.clone(), "")]);
    let out = waiting(
        &stub.origin,
        "getBackupRestore",
        "backupRestoreId=bkr_123",
        &["--timeout", "1", "--interval", "10"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), pending);
    assert!(String::from_utf8_lossy(&out.stderr).contains("timed out"));
    assert_eq!(stub.finish().len(), 1);
}

#[test]
fn restore_creation_preserves_selection_job_and_polling_location() {
    let body = json!({"object_type":"JOB","job_type":"BACKUP_RESTORE","data":{"object_type":"BACKUP_RESTORE","id":"bkr_123","backup_id":"bkp_123","language_codes":["en","de"],"status":"QUEUED"}});
    let stub = Stub::new(vec![response(
        "202 Accepted",
        body.clone(),
        "Location: /v2/backup-restores/bkr_123\r\n",
    )]);
    let input = "{\"backup_id\":\"bkp_123\",\"language_codes\":[\"en\",\"de\"]}";
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "--idempotency-key",
            "restore-once",
            "api",
            "call",
            "createBackupRestore",
            "--path",
            "branchId=prb_123",
            "--file",
            "-",
        ],
        Some(input),
    );
    assert_eq!(success(&out), body);
    assert!(String::from_utf8_lossy(&out.stderr).contains("location: /v2/backup-restores/bkr_123"));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("POST /v2/branches/prb_123/backup-restores "));
    assert!(requests[0].contains(input));
    assert!(
        requests[0]
            .to_lowercase()
            .contains("idempotency-key: restore-once")
    );
}

#[test]
fn restore_creation_preserves_source_field_error_without_retry() {
    let problem = json!({"title":"Invalid restore source","code":"validation_failed","request_id":"restore-request","errors":[{"pointer":"#/backup_id","code":"not_found","detail":"Backup not found in this project"}]});
    let stub = Stub::new(vec![response("422 Unprocessable Entity", problem.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "createBackupRestore",
            "--path",
            "branchId=prb_123",
            "--file",
            "-",
        ],
        Some("{\"backup_id\":\"bkp_123\",\"language_codes\":[\"en\"]}"),
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let error: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(error["problem"], problem);
    assert_eq!(stub.finish().len(), 1);
}

#[test]
fn backup_settings_discovery_preserves_disabled_and_configured_state() {
    for enabled in [false, true] {
        let body = json!({"object_type":"BACKUP_SETTINGS","id":"pr_123","is_enabled":enabled,"schedule":if enabled {json!({"period":"WEEKLY","time":"08:30","day_of_week":"MONDAY"})}else{Value::Null},"branch_ids":if enabled{json!(["prb_123"])}else{json!([])},"created_at":null,"updated_at":null,"next_execution_at":null});
        let stub = Stub::new(vec![response("200 OK", body.clone(), "ETag: \"schedule\"\r\n")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                "getBackupSettings",
                "--path",
                "projectId=pr_123",
            ],
            None,
        );
        assert_eq!(success(&out), body);
        assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"schedule\""));
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /v2/projects/pr_123/backup-settings "));
    }
}

#[test]
fn backup_settings_discovery_preserves_scope_and_permission_errors() {
    for (status, code) in [
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let problem = json!({"title":"Settings unavailable","code":code,"request_id":"backup-settings"});
        let stub = Stub::new(vec![response(status, problem.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "getBackupSettings",
                "--path",
                "projectId=pr_123",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let error: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(error["problem"], problem);
        assert_eq!(stub.finish().len(), 1);
    }
}

fn entry_download(origin: &str, output: &std::path::Path, extras: &[&str]) -> Output {
    let mut args = vec![
        "--json",
        "api",
        "download-export-entry",
        "reg_123",
        "ree_selected",
        "--output",
        output.to_str().unwrap(),
    ];
    args.extend_from_slice(extras);
    command(origin, &args, None)
}
fn entry_metadata(status: &str, url: Value) -> Value {
    json!({"object_type":"EXPORT_ENTRY","id":"ree_selected","status":status,"download_url":url})
}
fn entry_page(data: Value, cursor: Value) -> Value {
    json!({"object_type":"LIST","data":data,"has_more":!cursor.is_null(),"next_cursor":cursor})
}

#[test]
fn entry_download_traverses_empty_and_unrelated_pages_and_downloads_only_the_requested_entry() {
    let fixture = UploadFixture::new("marker", b"preserve");
    let destination = fixture.0.with_file_name("selected.json");
    let bytes = "{\"greeting\":\"Grüße — selected file\"}".as_bytes();
    let artifact = Stub::bytes(vec![artifact_response(bytes)]);
    let selected = entry_metadata(
        "SUCCEEDED",
        json!(format!("{}/selected?secret-link", artifact.origin)),
    );
    let api = Stub::new(vec![
        response(
            "200 OK",
            entry_page(json!([]), json!("first/+==")),
            "X-Request-ID: first-page\r\n",
        ),
        response(
            "200 OK",
            entry_page(
                json!([{"object_type":"EXPORT_ENTRY","id":"ree_other","status":"FAILED","download_url":null}]),
                json!("second"),
            ),
            "",
        ),
        response(
            "200 OK",
            entry_page(json!([selected]), Value::Null),
            "X-Request-ID: selected-page\r\n",
        ),
    ]);
    let out = entry_download(&api.origin, &destination, &["--include-headers"]);
    let result = success(&out);
    assert_eq!(result["export_id"], "reg_123");
    assert_eq!(result["export_entry_id"], "ree_selected");
    assert_eq!(result["bytes_written"], bytes.len());
    assert_eq!(std::fs::read(&destination).unwrap(), bytes);
    let requests = api.finish();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        let target = request.split_whitespace().nth(1).unwrap();
        let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
        assert_eq!(url.path(), "/v2/exports/reg_123/entries");
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "limit" && value == "100")
        );
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer test-secret-do-not-print")
        );
    }
    let second = reqwest::Url::parse(&format!(
        "http://localhost{}",
        requests[1].split_whitespace().nth(1).unwrap()
    ))
    .unwrap();
    assert!(
        second
            .query_pairs()
            .any(|(key, value)| key == "cursor" && value == "first/+==")
    );
    let raw = &artifact.finish()[0];
    assert!(raw.starts_with("GET /selected?secret-link "));
    for header in [
        "authorization:",
        "cookie:",
        "if-match:",
        "idempotency-key:",
        "referer:",
    ] {
        assert!(!raw.to_lowercase().contains(header));
    }
    assert!(String::from_utf8_lossy(&out.stderr).contains("first-page"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("selected-page"));
    no_download_leaks(&out);
    assert_no_staging(&fixture);
}

#[test]
fn entry_download_rejects_missing_unsuccessful_or_invalid_entries_without_artifact_access() {
    let trap = Stub::new(vec![]);
    let mut cases = vec![
        (
            entry_page(json!([]), Value::Null),
            "Export entry was not found in the requested export",
        ),
        (
            entry_page(
                json!([{"object_type":"EXPORT","id":"ree_selected","status":"SUCCEEDED","download_url":format!("{}/?secret-link", trap.origin)}]),
                Value::Null,
            ),
            "Expected export entry identities",
        ),
        (
            entry_page(json!([{"object_type":"EXPORT_ENTRY","id":null}]), Value::Null),
            "Expected export entry identities",
        ),
        (
            entry_page(
                json!([
                    entry_metadata("SUCCEEDED", Value::Null),
                    entry_metadata("SUCCEEDED", Value::Null)
                ]),
                Value::Null,
            ),
            "Server repeated the requested export entry",
        ),
        (
            json!({"object_type":"LIST","data":[],"has_more":true,"next_cursor":null}),
            "Invalid V2 collection: expected data, has_more and matching next_cursor",
        ),
    ];
    for status in [
        "QUEUED",
        "PROCESSING",
        "FAILED",
        "SKIPPED",
        "PARTIALLY_SUCCEEDED",
        "UNKNOWN",
    ] {
        cases.push((
            entry_page(
                json!([entry_metadata(
                    status,
                    json!(format!("{}/?secret-link", trap.origin))
                )]),
                Value::Null,
            ),
            "Export entry did not succeed; inspect its status before downloading",
        ));
    }
    cases.push((
        entry_page(json!([entry_metadata("SUCCEEDED", Value::Null)]), Value::Null),
        "Completed export or entry has no download URL",
    ));
    for url in [
        json!("http://example.com/?secret-link"),
        json!("https://user:secret-link@example.com/"),
        json!("https://example.com/#secret-link"),
    ] {
        cases.push((
            entry_page(json!([entry_metadata("SUCCEEDED", url)]), Value::Null),
            "Artifact download URL must use HTTPS (or HTTP loopback for local testing), without embedded credentials or a fragment",
        ));
    }
    for (body, expected_error) in cases {
        let fixture = UploadFixture::new("marker", b"preserve");
        let destination = fixture.0.with_file_name("selected.json");
        let api = Stub::new(vec![response("200 OK", body, "")]);
        let out = entry_download(&api.origin, &destination, &[]);
        assert_download_metadata_error(&out, expected_error);
        assert!(!destination.exists());
        assert_eq!(api.finish().len(), 1);
        assert_no_staging(&fixture);
        no_download_leaks(&out);
    }
    trap.finish();
}

#[test]
fn entry_download_stops_on_repeated_cursors_or_its_page_bound() {
    for repeated in [false, true] {
        let fixture = UploadFixture::new("marker", b"preserve");
        let destination = fixture.0.with_file_name("selected.json");
        let mut responses = vec![response("200 OK", entry_page(json!([]), json!("same")), "")];
        if repeated {
            responses.push(responses[0].clone());
        }
        let api = Stub::new(responses);
        let extras = if repeated {
            vec![]
        } else {
            vec!["--max-pages", "1"]
        };
        let out = entry_download(&api.origin, &destination, &extras);
        assert_eq!(out.status.code(), Some(1));
        let message = String::from_utf8_lossy(&out.stderr);
        assert!(message.contains(if repeated { "repeated" } else { "--max-pages" }));
        assert_eq!(api.finish().len(), if repeated { 2 } else { 1 });
        assert!(!destination.exists());
        assert_no_staging(&fixture);
    }
}

#[test]
fn entry_download_preserves_api_errors_existing_files_and_rejects_invalid_arguments() {
    let fixture = UploadFixture::new("existing.json", b"preserve");
    assert_eq!(
        entry_download("http://127.0.0.1:1", &fixture.0, &[])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(std::fs::read(&fixture.0).unwrap(), b"preserve");
    let destination = fixture.0.with_file_name("new.json");
    for extra in [
        vec!["--field", "x=y"],
        vec!["--file", "-"],
        vec!["--if-match", "etag"],
        vec!["--idempotency-key", "key"],
        vec!["--max-pages", "0"],
        vec!["--max-pages", "10001"],
        vec!["--max-bytes", "0"],
    ] {
        assert_eq!(
            entry_download("http://127.0.0.1:1", &destination, &extra)
                .status
                .code(),
            Some(2)
        );
    }
    let problem = json!({"status":404,"code":"resource_not_found","title":"Not found"});
    let api = Stub::new(vec![response(
        "404 Not Found",
        problem.clone(),
        "X-Request-ID: entry-denied\r\n",
    )]);
    let out = entry_download(&api.origin, &destination, &[]);
    assert_eq!(out.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(error["problem"], problem);
    assert_eq!(error["request_id"], "entry-denied");
    api.finish();
    assert!(!destination.exists());
    assert_no_staging(&fixture);
}

#[test]
fn entry_download_cleans_failed_redirected_truncated_and_oversized_artifacts() {
    let trap = Stub::new(vec![]);
    for raw in [
        "HTTP/1.1 302 Found\r\nConnection: close\r\nLocation: https://invalid.test/?secret-link\r\nContent-Length: 0\r\n\r\n",
        "HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 21\r\n\r\nprovider-private-body",
        "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 9\r\n\r\nshort",
        "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 12\r\n\r\n0123456789ab",
        "HTTP/1.1 200 OK\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n01234\r\n6\r\n56789a\r\n0\r\n\r\n",
    ] {
        let fixture = UploadFixture::new("marker", b"preserve");
        let destination = fixture.0.with_file_name("selected.json");
        let artifact = Stub::new(vec![raw.replace("https://invalid.test", &trap.origin)]);
        let api = Stub::new(vec![response(
            "200 OK",
            entry_page(
                json!([entry_metadata(
                    "SUCCEEDED",
                    json!(format!("{}/file?secret-link", artifact.origin))
                )]),
                Value::Null,
            ),
            "",
        )]);
        let out = entry_download(&api.origin, &destination, &["--max-bytes", "10"]);
        assert_eq!(out.status.code(), Some(1));
        assert!(!destination.exists());
        assert_no_staging(&fixture);
        no_download_leaks(&out);
        api.finish();
        artifact.finish();
    }
    trap.finish();
}

#[test]
fn entry_download_preserves_a_destination_created_during_transfer() {
    let fixture = UploadFixture::new("marker", b"preserve");
    let destination = fixture.0.with_file_name("selected.json");
    let competing = destination.clone();
    let artifact = Stub::with_hook(vec![artifact_response(b"new data")], move |_| {
        std::fs::write(&competing, b"existing data").unwrap();
    });
    let api = Stub::new(vec![response(
        "200 OK",
        entry_page(
            json!([entry_metadata(
                "SUCCEEDED",
                json!(format!("{}/file", artifact.origin))
            )]),
            Value::Null,
        ),
        "",
    )]);
    let out = entry_download(&api.origin, &destination, &[]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(std::fs::read(&destination).unwrap(), b"existing data");
    assert_no_staging(&fixture);
    api.finish();
    artifact.finish();
}

#[test]
fn effective_member_permissions_preserve_computed_access_and_validator() {
    let body = json!({"object_type":"STACK_MEMBER_PERMISSIONS","id":"wsu_123","stack_id":"stack_123","permissions":[{"scope":"STACK","target":"SEGMENT_TEXTS","level":"VIEW"}],"language_permissions":{"de":"READ_ACCESS","en":"NO_ACCESS"}});
    let stub = Stub::new(vec![response("200 OK", body.clone(), "ETag: \"effective\"\r\n")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "getStackMemberPermissions",
            "--path",
            "stackId=stack_123",
            "--path",
            "memberId=wsu_123",
        ],
        None,
    );
    assert_eq!(success(&out), body);
    assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"effective\""));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /v2/stacks/stack_123/member-permissions/wsu_123 "));
}

#[test]
fn effective_member_permissions_preserve_current_access_denials() {
    for (status, code) in [
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let problem =
            json!({"title":"Member access unavailable","code":code,"request_id":"member-permissions"});
        let stub = Stub::new(vec![response(status, problem.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "getStackMemberPermissions",
                "--path",
                "stackId=stack_123",
                "--path",
                "memberId=wsu_123",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let error: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(error["problem"], problem);
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn backup_deletion_sends_validator_and_accepts_no_content() {
    let stub = Stub::new(vec![
        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
    ]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "deleteBackup",
            "--path",
            "backupId=bkp_123",
            "--if-match",
            "\"snapshot\"",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("DELETE /v2/backups/bkp_123 "));
    assert!(requests[0].to_lowercase().contains("if-match: \"snapshot\""));
}

#[test]
fn backup_deletion_preserves_conflict_precondition_and_current_access_errors() {
    for (status, code) in [
        ("409 Conflict", "backup_in_use"),
        ("412 Precondition Failed", "precondition_failed"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let problem = json!({"title":"Backup unavailable","code":code,"request_id":"backup-delete"});
        let stub = Stub::new(vec![response(status, problem.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "deleteBackup",
                "--path",
                "backupId=bkp_123",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let error: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(error["problem"], problem);
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn zip_upload_keeps_archive_filename_and_plain_path_without_forcing_one_language() {
    let archive = include_bytes!("fixtures/nested-languages.zip");
    let file = UploadFixture::new("multilingual bundle.ZIP", archive);
    let job = json!({"object_type":"JOB","job_type":"IMPORT","data":{"id":"imp_zip","status":"QUEUED"}});
    let stub = Stub::new(vec![response(
        "202 Accepted",
        job.clone(),
        "Location: /v2/imports/imp_zip\r\n",
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "upload",
            "createImport",
            "--path",
            "branchId=prb_123",
            "--upload",
            file.path(),
            "--form",
            "path=src/bundle",
            "--idempotency-key",
            "zip-once",
        ],
        None,
    );
    assert_eq!(success(&out), job);
    let requests = stub.finish_bytes();
    assert_eq!(requests.len(), 1);
    let request = String::from_utf8_lossy(&requests[0]);
    assert!(request.starts_with("POST /v2/branches/prb_123/imports "));
    assert!(request.contains("filename=\"multilingual bundle.ZIP\""));
    assert!(
        request.contains("name=\"path\"\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nsrc/bundle\r\n")
    );
    assert!(requests[0].windows(archive.len()).any(|window| window == archive));
    assert!(!request.contains("name=\"language_code\""));
    assert!(!request.contains(file.path()));
}

#[test]
fn zip_validation_errors_preserve_field_details_without_retry() {
    let file = UploadFixture::new("archive.zip", include_bytes!("fixtures/nested-languages.zip"));
    for code in ["invalid_archive", "empty_archive", "file_too_large"] {
        let problem = json!({"title":"Invalid archive","code":"validation_failed","errors":[{"parameter":"file","code":code}],"request_id":"zip-validation"});
        let stub = Stub::new(vec![response("422 Unprocessable Entity", problem.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "upload",
                "createImport",
                "--path",
                "branchId=prb_123",
                "--upload",
                file.path(),
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let error: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(error["problem"], problem);
        assert_eq!(stub.finish_bytes().len(), 1);
    }
}

#[test]
fn integration_authentication_discovery_preserves_metadata_filters_and_project_link() {
    let id = "iau_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let body = json!({"object_type":"INTEGRATION_AUTHENTICATION","id":id,"name":"Company Git — Grüße","provider":"GITHUB","type":"INSTALLATION","status":"SUSPENDED","is_self_hosted":false,"provider_origin":"https://github.com","provider_region":null,"created_at":"2026-09-30T09:00:00.123Z","updated_at":"2026-09-30T09:00:00.123Z"});
    let page = json!({"object_type":"LIST","has_more":false,"next_cursor":null,"data":[body.clone()]});
    let filter = r#"{"where":{"_and":[{"integration_authentication[provider]":{"_eq":"GITHUB"}}]}}"#;
    let stub = Stub::new(vec![response("200 OK", page.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listIntegrationAuthentications",
            "--query",
            &format!("filter={filter}"),
            "--query",
            "limit=1",
        ],
        None,
    );
    assert_eq!(success(&out), page);
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    let query = reqwest::Url::parse(&format!(
        "http://localhost{}",
        requests[0].split_whitespace().nth(1).unwrap()
    ))
    .unwrap();
    assert_eq!(query.path(), "/v2/integration-authentications");
    assert!(
        query
            .query_pairs()
            .any(|(key, value)| key == "filter" && value == filter)
    );
    let stub = Stub::new(vec![response(
        "200 OK",
        body.clone(),
        "ETag: \"auth-current\"\r\n",
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "getIntegrationAuthentication",
            "--path",
            &format!("integrationAuthenticationId={id}"),
        ],
        None,
    );
    assert_eq!(success(&out), body);
    assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"auth-current\""));
    assert!(stub.finish()[0].starts_with(&format!("GET /v2/integration-authentications/{id} ")));
    let project = json!({"object_type":"PROJECT_GIT_INTEGRATION","id":"pr_123","git_authentication_id":id});
    let stub = Stub::new(vec![response("200 OK", project.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "getProjectGitIntegration",
            "--path",
            "projectId=pr_123",
        ],
        None,
    );
    assert_eq!(success(&out), project);
    assert_eq!(stub.finish().len(), 1);
}

#[test]
fn integration_authentication_reads_preserve_access_and_absence_problems_without_retry() {
    for (status, code) in [
        ("401 Unauthorized", "invalid_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        for single in [false, true] {
            let body = json!({"title":"Unavailable","code":code,"request_id":"git-auth-discovery"});
            let stub = Stub::new(vec![response(status, body.clone(), "")]);
            let mut args = vec![
                "--json",
                "api",
                "call",
                if single {
                    "getIntegrationAuthentication"
                } else {
                    "listIntegrationAuthentications"
                },
            ];
            if single {
                args.extend(["--path", "integrationAuthenticationId=iau_123"]);
            }
            let out = command(&stub.origin, &args, None);
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            assert_eq!(
                serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
                body
            );
            assert_eq!(stub.finish().len(), 1);
        }
    }
}

#[test]
fn audit_discovery_preserves_public_metadata_filters_cursors_and_validators() {
    let id = "audit_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let item = json!({"object_type":"AUDIT_LOG","id":id,"category":"PROJECTS","target":"GENERAL","event":"UPDATE",
        "actor_id":"act_0197c8f2-5a6b-7cde-8f12-3456789abcde","actor_type":"API_KEY","actor_name":"Deleted API Key",
        "project_id":null,"project_name":"Website — Grüße","created_at":"2026-09-30T10:00:00.000Z","updated_at":"2026-09-30T10:00:00.000Z"});
    let filter = json!({"where":{"_and":[{"audit_log[event]":{"_eq":"UPDATE"}}]}}).to_string();
    let stub = Stub::new(vec![
        response("200 OK", page(json!([]), Some("-audit+/=")), ""),
        response("200 OK", page(json!([item.clone()]), None), ""),
    ]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listAuditLogs",
            "--query",
            &format!("filter={filter}"),
            "--query",
            "limit=1",
        ],
        None,
    );
    let first = success(&out);
    assert_eq!(first, page(json!([]), Some("-audit+/=")));
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listAuditLogs",
            "--query",
            &format!("filter={filter}"),
            "--query",
            "limit=1",
            "--query",
            &format!("cursor={}", first["next_cursor"].as_str().unwrap()),
        ],
        None,
    );
    assert_eq!(success(&out), page(json!([item.clone()]), None));
    let requests = stub.finish();
    for raw in &requests {
        let target = raw.split_whitespace().nth(1).unwrap();
        let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
        assert_eq!(url.path(), "/v2/audit-logs");
        assert!(url.query_pairs().any(|(k, v)| k == "filter" && v == filter));
    }
    let url = reqwest::Url::parse(&format!(
        "http://localhost{}",
        requests[1].split_whitespace().nth(1).unwrap()
    ))
    .unwrap();
    assert!(url.query_pairs().any(|(k, v)| k == "cursor" && v == "-audit+/="));
    let stub = Stub::new(vec![response("200 OK", item.clone(), "ETag: \"audit\"\r\n")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "getAuditLog",
            "--path",
            &format!("auditLogId={id}"),
        ],
        None,
    );
    assert_eq!(success(&out), item);
    assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"audit\""));
    assert!(stub.finish()[0].starts_with(&format!("GET /v2/audit-logs/{id} ")));
}

#[test]
fn audit_discovery_preserves_retention_permission_and_filter_problems_without_retry() {
    for (status, code, single) in [
        ("401 Unauthorized", "disabled_api_key", false),
        ("403 Forbidden", "insufficient_permissions", true),
        ("404 Not Found", "resource_not_found", true),
        ("400 Bad Request", "invalid_filter", false),
    ] {
        let problem =
            json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"audit-request"});
        let stub = Stub::new(vec![response(status, problem.clone(), "")]);
        let args = if single {
            vec![
                "--json",
                "api",
                "call",
                "getAuditLog",
                "--path",
                "auditLogId=audit_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ]
        } else {
            vec!["--json", "api", "call", "listAuditLogs"]
        };
        let out = command(&stub.origin, &args, None);
        assert_eq!(out.status.code(), Some(1));
        let error: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(error["problem"], problem);
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn historical_json_numbers_round_trip_without_float_conversion() {
    let body = r#"{"budget":12345678901234567890.123456789,"quantity":123456789012345678901234567890,"original":null,"small":0.0000000000000000000123456789}"#;
    let raw = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let stub = Stub::new(vec![raw]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "request",
            "GET",
            "/v2/audit-logs/audit_fixture/changes",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    for number in [
        "12345678901234567890.123456789",
        "123456789012345678901234567890",
        "0.0000000000000000000123456789",
    ] {
        assert!(text.contains(number), "{text}");
    }
    assert_eq!(stub.finish().len(), 1);
}

#[test]
fn audit_changes_preserve_availability_nulls_omissions_and_historical_identity() {
    let id = "audit_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let body = json!({"object_type":"AUDIT_LOG_CHANGES","id":id,"is_available":true,"omitted_datasets":0,"datasets":[{"entity_type":"WEBHOOK","is_available":true,"entity_id":"whe_0197c8f2-5a6b-7cde-8f12-3456789abcde","identity_availability":"AVAILABLE","omitted_entries":1,"entries":[{"name":"secret","is_changed":true,"value":{"availability":"REDACTED","value":null},"original":{"availability":"REDACTED","value":null}},{"name":"description","is_changed":null,"value":{"availability":"AVAILABLE","value":null},"original":{"availability":"UNAVAILABLE","value":null}}]}],"created_at":"2026-09-30T12:00:00.123Z","updated_at":"2026-09-30T12:00:00.123Z"});
    let stub = Stub::new(vec![response("200 OK", body.clone(), "ETag: \"changes\"\r\n")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "getAuditLogChanges",
            "--path",
            &format!("auditLogId={id}"),
        ],
        None,
    );
    assert_eq!(success(&out), body);
    assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"changes\""));
    assert!(stub.finish()[0].starts_with(&format!("GET /v2/audit-logs/{id}/changes ")));
}

#[test]
fn audit_changes_preserve_current_access_failures_without_retry() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let body = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"audit-changes-request"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "getAuditLogChanges",
                "--path",
                "auditLogId=audit_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn invitation_resend_uses_bodyless_action_current_validator_and_idempotency_key() {
    let id = "svi_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let body = json!({"object_type":"INVITATION","id":id,"status":"PENDING","expires_at":"2026-10-07T10:00:00.123Z","message":null});
    let stub = Stub::new(vec![response("200 OK", body.clone(), "ETag: \"renewed\"\r\n")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "resendInvitation",
            "--path",
            &format!("invitationId={id}"),
            "--if-match",
            "\"current\"",
            "--idempotency-key",
            "resend-once",
        ],
        None,
    );
    assert_eq!(success(&out), body);
    assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"renewed\""));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!("POST /v2/invitations/{id}/resend ")));
    let headers = requests[0].to_lowercase();
    assert!(headers.contains("if-match: \"current\""));
    assert!(headers.contains("idempotency-key: resend-once"));
    assert!(requests[0].split_once("\r\n\r\n").unwrap().1.is_empty());
}

#[test]
fn invitation_resend_keeps_conflict_visibility_and_uncertainty_without_automatic_retry() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("409 Conflict", "conflict"),
        ("409 Conflict", "owner_membership_protected"),
        ("409 Conflict", "idempotency_outcome_unknown"),
        ("412 Precondition Failed", "precondition_failed"),
        ("500 Internal Server Error", "internal_error"),
    ] {
        let body =
            json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"resend-request"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "resendInvitation",
                "--path",
                "invitationId=svi_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--idempotency-key",
                "resend-once",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn workspace_api_key_inventory_preserves_public_metadata_filters_cursors_and_nulls() {
    let id = "api_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let body = json!({"object_type":"WORKSPACE_API_KEY","id":id,"workspace_id":"space_0197c8f2-5a6b-7cde-8f12-3456789abcde","name":"Retained key","description":null,"management_mode":"USER_OWNED","scope":"USER","stack_id":null,"member_id":null,"user_id":null,"expires_at":"2026-10-01T00:00:00.000Z","last_used_at":null,"is_deactivated":true,"deactivated_at":"2026-09-30T13:00:00.123Z","deactivation_reason":"WORKSPACE_USER_REMOVED","created_by_actor_id":"act_0197c8f2-5a6b-7cde-8f12-3456789abcde","deactivated_by_actor_id":null,"created_at":"2026-09-01T10:00:00.000Z","updated_at":"2026-09-01T10:00:00.000Z"});
    let page = json!({"object_type":"LIST","data":[body.clone()],"has_more":true,"next_cursor":"opaque+/="});
    let filter = r#"{"where":{"_and":[{"workspace_api_key[is_deactivated]":{"_true":true}}]}}"#;
    let stub = Stub::new(vec![
        response("200 OK", page.clone(), ""),
        response("200 OK", body.clone(), "ETag: \"retained\"\r\n"),
    ]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listWorkspaceApiKeys",
            "--query",
            "limit=1",
            "--query",
            "cursor=previous+/=",
            "--query",
            &format!("filter={filter}"),
        ],
        None,
    );
    assert_eq!(success(&out), page);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "getWorkspaceApiKey",
            "--path",
            &format!("apiKeyId={id}"),
        ],
        None,
    );
    assert_eq!(success(&out), body);
    assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"retained\""));
    let requests = stub.finish();
    assert_eq!(requests.len(), 2);
    let url = reqwest::Url::parse(&format!(
        "http://localhost{}",
        requests[0].split_whitespace().nth(1).unwrap()
    ))
    .unwrap();
    assert_eq!(url.path(), "/v2/api-keys");
    assert!(url.query_pairs().any(|(k, v)| k == "filter" && v == filter));
    assert!(
        url.query_pairs()
            .any(|(k, v)| k == "cursor" && v == "previous+/=")
    );
    assert!(url.query_pairs().any(|(k, v)| k == "limit" && v == "1"));
    assert!(requests[1].starts_with(&format!("GET /v2/api-keys/{id} ")));
}

#[test]
fn workspace_api_key_inventory_preserves_current_permission_and_visibility_failures() {
    for operation in ["listWorkspaceApiKeys", "getWorkspaceApiKey"] {
        for (status, code) in [
            ("401 Unauthorized", "disabled_api_key"),
            ("403 Forbidden", "insufficient_permissions"),
            ("404 Not Found", "resource_not_found"),
            ("400 Bad Request", "invalid_cursor"),
        ] {
            let body = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"inventory-request"});
            let stub = Stub::new(vec![response(status, body.clone(), "")]);
            let mut args = vec!["--json", "api", "call", operation];
            if operation == "getWorkspaceApiKey" {
                args.extend(["--path", "apiKeyId=api_0197c8f2-5a6b-7cde-8f12-3456789abcde"]);
            }
            let out = command(&stub.origin, &args, None);
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            assert_eq!(
                serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
                body
            );
            assert_eq!(stub.finish().len(), 1);
        }
    }
}

#[test]
fn workspace_api_key_revocation_uses_bodyless_delete_and_preserves_empty_success() {
    let id = "api_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let stub = Stub::new(vec![
        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
    ]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "deleteWorkspaceApiKey",
            "--path",
            &format!("apiKeyId={id}"),
            "--if-match",
            "\"current-key\"",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty());
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!("DELETE /v2/api-keys/{id} ")));
    assert!(requests[0].to_lowercase().contains("if-match: \"current-key\""));
    assert!(requests[0].split_once("\r\n\r\n").unwrap().1.is_empty());
}

#[test]
fn workspace_api_key_revocation_preserves_authentication_permission_and_stale_failures() {
    for (status, code) in [
        ("401 Unauthorized", "revoked_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("412 Precondition Failed", "precondition_failed"),
    ] {
        let body = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"revocation-request"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "deleteWorkspaceApiKey",
                "--path",
                "apiKeyId=api_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--if-match",
                "\"current-key\"",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn workspace_api_key_rotation_preserves_original_credential_without_diagnostic_disclosure() {
    let id = "api_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let token = "lh-api_synthetic_new_credential";
    let body = json!({"object_type":"WORKSPACE_API_KEY_TOKEN","id":id,"token":token,"created_at":"2026-09-30T12:00:00.123Z","updated_at":"2026-09-30T13:00:00.123Z"});
    let stub = Stub::new(vec![response(
        "200 OK",
        body.clone(),
        "Cache-Control: no-store\r\n",
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "rotateWorkspaceApiKeyToken",
            "--path",
            &format!("apiKeyId={id}"),
            "--if-match",
            "\"current\"",
            "--idempotency-key",
            "rotate-once",
        ],
        None,
    );
    assert_eq!(success(&out), body);
    assert!(!String::from_utf8_lossy(&out.stderr).contains(token));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!("POST /v2/api-keys/{id}/rotate-token ")));
    let headers = requests[0].to_lowercase();
    assert!(headers.contains("if-match: \"current\""));
    assert!(headers.contains("idempotency-key: rotate-once"));
    assert!(requests[0].split_once("\r\n\r\n").unwrap().1.is_empty());
}

#[test]
fn workspace_api_key_rotation_never_retries_uncertain_or_rejected_actions() {
    for (status, code) in [
        ("401 Unauthorized", "revoked_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("409 Conflict", "idempotency_outcome_unknown"),
        ("412 Precondition Failed", "precondition_failed"),
    ] {
        let body =
            json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"rotation-request"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "rotateWorkspaceApiKeyToken",
                "--path",
                "apiKeyId=api_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--idempotency-key",
                "rotate-once",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn api_key_permission_discovery_preserves_configuration_stack_permissions_cursors_and_validators() {
    let id = "api_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let stack = "stack_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let configuration = json!({"object_type":"API_KEY_PERMISSION_CONFIGURATION","id":id,"management_mode":"USER_OWNED","workspace_permissions":null,"default_stack_permissions":null,"created_at":"2026-09-30T00:00:00.123Z","updated_at":"2026-09-30T00:00:00.123Z"});
    let row = json!({"object_type":"API_KEY_STACK_PERMISSIONS","api_key_id":id,"stack_id":stack,"permissions":[{"scope":"STACK","target":"RESOURCES","level":"VIEW"}],"created_at":"2026-09-30T00:00:00.123Z","updated_at":"2026-09-30T00:00:00.123Z"});
    let page = json!({"object_type":"LIST","data":[row.clone()],"has_more":true,"next_cursor":"next+/="});
    let filter = r#"{"where":{"_and":[{"api_key_stack_permissions[stack_id]":{"_eq":"stack_0197c8f2-5a6b-7cde-8f12-3456789abcde"}}]}}"#;
    let stub = Stub::new(vec![
        response("200 OK", configuration.clone(), "ETag: \"configuration\"\r\n"),
        response("200 OK", page.clone(), ""),
        response("200 OK", row.clone(), "ETag: \"override\"\r\n"),
    ]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "getWorkspaceApiKeyPermissions",
            "--path",
            &format!("apiKeyId={id}"),
        ],
        None,
    );
    assert_eq!(success(&out), configuration);
    assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"configuration\""));
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listWorkspaceApiKeyStackPermissions",
            "--path",
            &format!("apiKeyId={id}"),
            "--query",
            "limit=1",
            "--query",
            "cursor=previous+/=",
            "--query",
            &format!("filter={filter}"),
        ],
        None,
    );
    assert_eq!(success(&out), page);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "getWorkspaceApiKeyStackPermissions",
            "--path",
            &format!("apiKeyId={id}"),
            "--path",
            &format!("stackId={stack}"),
        ],
        None,
    );
    assert_eq!(success(&out), row);
    assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"override\""));
    let requests = stub.finish();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].starts_with(&format!("GET /v2/api-keys/{id}/permissions ")));
    let url = reqwest::Url::parse(&format!(
        "http://localhost{}",
        requests[1].split_whitespace().nth(1).unwrap()
    ))
    .unwrap();
    assert_eq!(url.path(), format!("/v2/api-keys/{id}/stack-permissions"));
    assert!(
        url.query_pairs()
            .any(|(k, v)| k == "cursor" && v == "previous+/=")
    );
    assert!(url.query_pairs().any(|(k, v)| k == "filter" && v == filter));
    assert!(requests[2].starts_with(&format!("GET /v2/api-keys/{id}/stack-permissions/{stack} ")));
}

#[test]
fn api_key_permission_discovery_preserves_live_permission_visibility_and_query_failures() {
    for operation in [
        "getWorkspaceApiKeyPermissions",
        "listWorkspaceApiKeyStackPermissions",
        "getWorkspaceApiKeyStackPermissions",
    ] {
        for (status, code) in [
            ("401 Unauthorized", "disabled_api_key"),
            ("403 Forbidden", "insufficient_permissions"),
            ("404 Not Found", "resource_not_found"),
            ("400 Bad Request", "invalid_filter"),
        ] {
            let body = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"permissions-request"});
            let stub = Stub::new(vec![response(status, body.clone(), "")]);
            let mut args = vec![
                "--json",
                "api",
                "call",
                operation,
                "--path",
                "apiKeyId=api_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ];
            if operation == "getWorkspaceApiKeyStackPermissions" {
                args.extend(["--path", "stackId=stack_0197c8f2-5a6b-7cde-8f12-3456789abcde"]);
            }
            let out = command(&stub.origin, &args, None);
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            assert_eq!(
                serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
                body
            );
            assert_eq!(stub.finish().len(), 1);
        }
    }
}

#[test]
fn api_key_stack_permission_removal_uses_bodyless_delete_and_preserves_empty_success() {
    let api = "api_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let stack = "stack_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let stub = Stub::new(vec![
        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
    ]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "deleteWorkspaceApiKeyStackPermissions",
            "--path",
            &format!("apiKeyId={api}"),
            "--path",
            &format!("stackId={stack}"),
            "--if-match",
            "\"current-key\"",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty());
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!("DELETE /v2/api-keys/{api}/stack-permissions/{stack} ")));
    assert!(requests[0].to_lowercase().contains("if-match: \"current-key\""));
    assert!(requests[0].split_once("\r\n\r\n").unwrap().1.is_empty());
}

#[test]
fn api_key_stack_permission_removal_preserves_authentication_permission_and_stale_failures() {
    for (status, code) in [
        ("401 Unauthorized", "revoked_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("412 Precondition Failed", "precondition_failed"),
    ] {
        let body = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"revocation-request"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "deleteWorkspaceApiKeyStackPermissions",
                "--path",
                "apiKeyId=api_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--path",
                "stackId=stack_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--if-match",
                "\"current-key\"",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn context_image_segments_preserve_candidates_normalized_regions_and_cursors() {
    let image = "ci_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let row = json!({"object_type":"CONTEXT_IMAGE_SEGMENT","context_image_id":image,"segment_id":"seg_0197c8f2-5a6b-7cde-8f12-3456789abcde","language_code":"de","link_type":"SUGGESTION","highlight_boxes":[{"left":0.125,"top":0.25,"width":0.5,"height":0.1}],"is_highlighted":true,"created_at":"2026-09-30T00:00:00.123Z","updated_at":"2026-09-30T00:00:00.123Z"});
    let page = json!({"object_type":"LIST","data":[row],"has_more":true,"next_cursor":"next+/="});
    let filter = r#"{"where":{"_and":[{"context_image_segment[link_type]":{"_eq":"SUGGESTION"}}]}}"#;
    let stub = Stub::new(vec![response("200 OK", page.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listContextImageSegments",
            "--path",
            &format!("contextImageId={image}"),
            "--query",
            "limit=1",
            "--query",
            "cursor=previous+/=",
            "--query",
            &format!("filter={filter}"),
        ],
        None,
    );
    assert_eq!(success(&out), page);
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    let url = reqwest::Url::parse(&format!(
        "http://localhost{}",
        requests[0].split_whitespace().nth(1).unwrap()
    ))
    .unwrap();
    assert_eq!(url.path(), format!("/v2/context-images/{image}/segments"));
    assert!(url.query_pairs().any(|(k, v)| k == "filter" && v == filter));
    assert!(
        url.query_pairs()
            .any(|(k, v)| k == "cursor" && v == "previous+/=")
    );
    assert!(url.query_pairs().any(|(k, v)| k == "limit" && v == "1"));
}

#[test]
fn context_image_segments_preserve_empty_regions_and_permission_visibility_errors() {
    let path = "contextImageId=ci_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let page = json!({"object_type":"LIST","data":[{"highlight_boxes":[],"is_highlighted":true}],"has_more":false,"next_cursor":null});
    let stub = Stub::new(vec![response("200 OK", page.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listContextImageSegments",
            "--path",
            path,
        ],
        None,
    );
    assert_eq!(success(&out), page);
    assert_eq!(stub.finish().len(), 1);
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("400 Bad Request", "invalid_cursor"),
    ] {
        let body = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"segment-links-request"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "listContextImageSegments",
                "--path",
                path,
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn context_image_detach_posts_segment_ids_and_returns_the_updated_image() {
    let image = "ci_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let input =
        json!({"segment_ids":["seg_0197c8f2-5a6b-7cde-8f12-3456789abcde"],"include_suggestions":false});
    let updated = json!({"object_type":"CONTEXT_IMAGE","id":image,"attachments":[]});
    let stub = Stub::new(vec![response(
        "200 OK",
        updated.clone(),
        "ETag: \"detached\"\r\n",
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "detachContextImage",
            "--path",
            &format!("contextImageId={image}"),
            "--idempotency-key",
            "detach-once",
            "--file",
            "-",
        ],
        Some(&input.to_string()),
    );
    assert_eq!(success(&out), updated);
    assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"detached\""));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!("POST /v2/context-images/{image}/detach ")));
    let (headers, payload) = requests[0].split_once("\r\n\r\n").unwrap();
    let headers = headers.to_lowercase();
    assert!(headers.contains("idempotency-key: detach-once"));
    assert!(!headers.contains("if-match:"));
    assert_eq!(serde_json::from_str::<Value>(payload).unwrap(), input);
}

#[test]
fn context_image_detach_preserves_failures_without_retry() {
    for (status, code) in [
        ("401 Unauthorized", "revoked_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("409 Conflict", "idempotency_outcome_unknown"),
    ] {
        let body =
            json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"detach-image"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "detachContextImage",
                "--path",
                "contextImageId=ci_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--file",
                "-",
            ],
            Some(r#"{"segment_ids":["seg_0197c8f2-5a6b-7cde-8f12-3456789abcde"]}"#),
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn project_placeholder_settings_preserve_order_empty_null_and_future_profiles() {
    for definitions in [
        json!(["IOS_PRINTF", "DEFAULT_ICU", "IOS_PRINTF", "FUTURE_PROFILE"]),
        json!([]),
        Value::Null,
    ] {
        let body = json!({"object_type":"PROJECT_PLACEHOLDER_SETTINGS","id":"pr_0197c8f2-5a6b-7cde-8f12-3456789abcde","definitions":definitions,"default_definitions":["DEFAULT_ICU"],"is_default":false,"created_at":"2026-09-30T00:00:00.000Z","updated_at":"2026-09-30T00:00:00.000Z"});
        let stub = Stub::new(vec![response("200 OK", body.clone(), "ETag: \"profiles\"\r\n")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                "getProjectPlaceholderSettings",
                "--path",
                "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        assert!(String::from_utf8_lossy(&out.stderr).contains("profiles"));
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].starts_with(
                "GET /v2/projects/pr_0197c8f2-5a6b-7cde-8f12-3456789abcde/placeholder-settings "
            )
        );
    }
}

#[test]
fn project_placeholder_settings_preserve_current_permission_failures() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let body = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"placeholder-settings"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "getProjectPlaceholderSettings",
                "--path",
                "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn project_placeholder_reset_posts_without_body_and_preserves_conditional_headers() {
    let body = json!({"object_type":"PROJECT_PLACEHOLDER_SETTINGS","id":"pr_0197c8f2-5a6b-7cde-8f12-3456789abcde","definitions":["DEFAULT_ICU"],"default_definitions":["DEFAULT_ICU"],"is_default":true,"created_at":"2026-09-30T00:00:00.000Z","updated_at":"2026-09-30T00:01:00.000Z"});
    let stub = Stub::new(vec![response(
        "200 OK",
        body.clone(),
        "ETag: \"reset-profiles\"\r\n",
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "resetProjectPlaceholderSettings",
            "--path",
            "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            "--if-match",
            "\"profiles\"",
            "--idempotency-key",
            "reset-once",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
    assert!(String::from_utf8_lossy(&out.stderr).contains("reset-profiles"));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(
        "POST /v2/projects/pr_0197c8f2-5a6b-7cde-8f12-3456789abcde/placeholder-settings/reset "
    ));
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains("if-match: \"profiles\"")
    );
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains("idempotency-key: reset-once")
    );
    assert!(requests[0].split_once("\r\n\r\n").unwrap().1.is_empty());
}

#[test]
fn project_placeholder_reset_preserves_stale_and_current_permission_failures() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("412 Precondition Failed", "precondition_failed"),
    ] {
        let body = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"reset-placeholder-settings"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "resetProjectPlaceholderSettings",
                "--path",
                "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn project_import_export_settings_preserve_typed_and_empty_configuration() {
    for (operation, kind) in [
        ("getProjectImportSettings", "import"),
        ("getProjectExportSettings", "export"),
    ] {
        let configured = if kind == "import" {
            json!({"is_source_creation_enabled":false,"source_text_update_mode":"FUTURE_MODE","is_source_metadata_update_enabled":true,"is_source_deactivation_enabled":false,"is_target_creation_enabled":true,"target_text_update_mode":null,"is_target_metadata_update_enabled":false,"is_target_deactivation_enabled":true})
        } else {
            json!({"source_statuses":[],"target_statuses":null,"source_empty_content_handling":"KEEP","target_empty_content_handling":null,"is_not_translatable_skipped":true,"html_encoding":"FUTURE_ENCODING"})
        };
        let configuration = configured;
        let body = json!({"object_type":format!("PROJECT_{}_SETTINGS",kind.to_uppercase()),"id":"pr_0197c8f2-5a6b-7cde-8f12-3456789abcde","configuration":configuration,"created_at":"2026-09-30T00:00:00.000Z","updated_at":"2026-09-30T00:00:00.000Z"});
        let stub = Stub::new(vec![response(
            "200 OK",
            body.clone(),
            "ETag: \"file-settings\"\r\n",
        )]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                operation,
                "--path",
                "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        assert!(String::from_utf8_lossy(&out.stderr).contains("file-settings"));
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!(
            "GET /v2/projects/pr_0197c8f2-5a6b-7cde-8f12-3456789abcde/{kind}-settings "
        )));
    }
}

#[test]
fn project_import_export_settings_preserve_current_access_errors() {
    for operation in ["getProjectImportSettings", "getProjectExportSettings"] {
        for (status, code) in [
            ("401 Unauthorized", "disabled_api_key"),
            ("403 Forbidden", "insufficient_permissions"),
            ("404 Not Found", "resource_not_found"),
        ] {
            let body = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"project-file-settings"});
            let stub = Stub::new(vec![response(status, body.clone(), "")]);
            let out = command(
                &stub.origin,
                &[
                    "--json",
                    "api",
                    "call",
                    operation,
                    "--path",
                    "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                ],
                None,
            );
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            assert_eq!(
                serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
                body
            );
            assert_eq!(stub.finish().len(), 1);
        }
    }
}

#[test]
fn stack_automation_settings_preserve_order_nulls_and_future_vocabulary() {
    for rules in [
        json!([]),
        json!([
            {"rule_key":"0197c8f2-5a6b-7cde-8f12-3456789abcde","type":"LABEL","is_enabled":false,"trigger":"FUTURE_TRIGGER","scope":{"origin":"ANY","role":"ANY","language_codes":[]},"conditions":[],"action":{"type":"ADD_LABEL","target":{"selector":"TRIGGERING_TEXT","language_codes":[],"conditions":[]},"status":null,"label_id":null,"search":null,"replace":null,"is_placeholder_excluded":false}},
            {"rule_key":null,"type":"FUTURE_FAMILY","is_enabled":true,"trigger":"TEXT_UPDATED","scope":{"origin":"EDITOR","role":"SOURCE","language_codes":["en"]},"conditions":[],"action":{"type":"REPLACE_TEXT","target":{"selector":"FUTURE_TARGET","language_codes":["de"],"conditions":[]},"status":null,"label_id":null,"search":"Grüße","replace":"","is_placeholder_excluded":true}}
        ]),
    ] {
        let body = json!({"object_type":"STACK_AUTOMATION_SETTINGS","id":"stack_0197c8f2-5a6b-7cde-8f12-3456789abcde","rules":rules,"max_rules":0,"created_at":"2026-09-30T00:00:00.000Z","updated_at":"2026-09-30T00:00:00.000Z"});
        let stub = Stub::new(vec![response(
            "200 OK",
            body.clone(),
            "ETag: \"automations\"\r\n",
        )]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                "getStackAutomationSettings",
                "--path",
                "stackId=stack_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        assert!(String::from_utf8_lossy(&out.stderr).contains("automations"));
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].starts_with(
                "GET /v2/stacks/stack_0197c8f2-5a6b-7cde-8f12-3456789abcde/automation-settings "
            )
        );
    }
}

#[test]
fn stack_automation_settings_preserve_current_access_errors() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let body =
            json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"automations"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "getStackAutomationSettings",
                "--path",
                "stackId=stack_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn project_format_settings_preserve_absence_custom_values_and_future_enums() {
    for configuration in [
        json!(null),
        json!({"file":null}),
        json!({"file":{"general":{"charset":"FUTURE_CHARSET","file_name_languages":"NONE","is_source_used_when_missing":false,"comment_languages":null,"indentation":null,"is_final_newline_enabled":false},"csv":{"is_import_detection_enabled":false,"key_header":"Schlüssel","description_header":"","delimiter":";","quote":"'","is_status_exported":true},"escape":null,"json":{"is_array_indentation_enabled":true,"is_plural_order_alphabetical":false},"i18next_json":{"version":"FUTURE_VERSION","is_base_plural_exported":true},"xliff":null,"properties":{"plural_separator":""}}}),
    ] {
        let body = json!({"object_type":"PROJECT_FORMAT_SETTINGS","id":"pr_0197c8f2-5a6b-7cde-8f12-3456789abcde","configuration":configuration,"created_at":"2026-09-30T00:00:00.000Z","updated_at":"2026-09-30T00:00:00.000Z"});
        let stub = Stub::new(vec![response("200 OK", body.clone(), "ETag: \"formats\"\r\n")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                "getProjectFormatSettings",
                "--path",
                "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        assert!(String::from_utf8_lossy(&out.stderr).contains("formats"));
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .starts_with("GET /v2/projects/pr_0197c8f2-5a6b-7cde-8f12-3456789abcde/format-settings ")
        );
    }
}

#[test]
fn project_format_settings_preserve_current_access_errors() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let body = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"formats"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "getProjectFormatSettings",
                "--path",
                "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn project_key_settings_preserve_strategy_absence_and_future_values() {
    for strategy in [
        json!("FORMAT_SPECIFIC"),
        json!("ALL"),
        json!("NONE"),
        json!("FUTURE_STRATEGY"),
        json!(null),
    ] {
        let body = json!({"object_type":"PROJECT_KEY_SETTINGS","id":"pr_0197c8f2-5a6b-7cde-8f12-3456789abcde","normalization_strategy":strategy,"created_at":"2026-09-30T00:00:00.000Z","updated_at":"2026-09-30T00:00:00.000Z"});
        let stub = Stub::new(vec![response("200 OK", body.clone(), "ETag: \"keys\"\r\n")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                "getProjectKeySettings",
                "--path",
                "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        assert!(String::from_utf8_lossy(&out.stderr).contains("keys"));
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].starts_with("GET /v2/projects/pr_0197c8f2-5a6b-7cde-8f12-3456789abcde/key-settings ")
        );
    }
}

#[test]
fn project_key_settings_preserve_current_access_errors() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let body = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"keys"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "getProjectKeySettings",
                "--path",
                "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn distribution_configuration_preserves_nullable_ranges_and_future_enums() {
    let body = json!({"object_type":"DISTRIBUTION","id":"dtb_0197c8f2-5a6b-7cde-8f12-3456789abcde","project_id":"pr_0197c8f2-5a6b-7cde-8f12-3456789abcde","name":"Österreich","type":"FUTURE_TYPE","environment":"FUTURE_ENVIRONMENT","release_schedule":"FUTURE_SCHEDULE","is_fallback":false,"version_from":"","version_to":null,"created_at":"2026-09-30T00:00:00.000Z","updated_at":"2026-09-30T00:00:00.000Z"});
    let stub = Stub::new(vec![response(
        "200 OK",
        body.clone(),
        "ETag: \"distribution\"\r\n",
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "getDistribution",
            "--path",
            "distributionId=dtb_0197c8f2-5a6b-7cde-8f12-3456789abcde",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
    assert!(String::from_utf8_lossy(&out.stderr).contains("distribution"));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /v2/distributions/dtb_0197c8f2-5a6b-7cde-8f12-3456789abcde "));
}
#[test]
fn distribution_list_transmits_filter_cursor_and_preserves_page() {
    let body = json!({"object_type":"LIST","has_more":true,"next_cursor":"opaque-next","data":[{"id":"dtb_0197c8f2-5a6b-7cde-8f12-3456789abcde"}]});
    let filter = r#"{"where":{"_and":[{"distribution[environment]":{"_eq":"PRODUCTION"}}]}}"#;
    let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listDistributions",
            "--path",
            "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            "--query",
            "limit=1",
            "--query",
            "cursor=opaque-input",
            "--query",
            &format!("filter={filter}"),
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    let line = requests[0].lines().next().unwrap();
    assert!(line.starts_with("GET /v2/projects/pr_0197c8f2-5a6b-7cde-8f12-3456789abcde/distributions?"));
    assert!(line.contains("limit=1"));
    assert!(line.contains("cursor=opaque-input"));
    assert!(line.contains("filter="));
    assert!(line.contains("PRODUCTION"));
}
#[test]
fn distribution_reads_preserve_current_access_errors() {
    for (operation, path) in [
        (
            "getDistribution",
            "distributionId=dtb_0197c8f2-5a6b-7cde-8f12-3456789abcde",
        ),
        (
            "listDistributions",
            "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
        ),
    ] {
        for (status, code) in [
            ("401 Unauthorized", "disabled_api_key"),
            ("403 Forbidden", "insufficient_permissions"),
            ("404 Not Found", "resource_not_found"),
        ] {
            let body =
                json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"distribution"});
            let stub = Stub::new(vec![response(status, body.clone(), "")]);
            let out = command(
                &stub.origin,
                &["--json", "api", "call", operation, "--path", path],
                None,
            );
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            assert_eq!(
                serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
                body
            );
            assert_eq!(stub.finish().len(), 1);
        }
    }
}

#[test]
fn distribution_release_metadata_preserves_nullable_counts_and_future_status() {
    let body = json!({"object_type":"DISTRIBUTION_RELEASE","id":"rel_0197c8f2-5a6b-7cde-8f12-3456789abcde","distribution_id":"dtb_0197c8f2-5a6b-7cde-8f12-3456789abcde","project_id":"pr_0197c8f2-5a6b-7cde-8f12-3456789abcde","name":"Österreich","branch_name":"deleted-main","status":"FUTURE_STATUS","language_codes":["en","de"],"file_count":null,"archive_size_kib":42,"activated_at":null,"created_at":"2026-09-30T00:00:00.000Z","updated_at":"2026-09-30T00:00:00.000Z"});
    let stub = Stub::new(vec![response(
        "200 OK",
        body.clone(),
        "ETag: \"distribution\"\r\n",
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "getDistributionRelease",
            "--path",
            "distributionReleaseId=rel_0197c8f2-5a6b-7cde-8f12-3456789abcde",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
    assert!(String::from_utf8_lossy(&out.stderr).contains("distribution"));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].starts_with("GET /v2/distribution-releases/rel_0197c8f2-5a6b-7cde-8f12-3456789abcde ")
    );
}
#[test]
fn distribution_release_list_transmits_status_filter_and_cursor() {
    let body = json!({"object_type":"LIST","has_more":true,"next_cursor":"opaque-next","data":[{"id":"rel_0197c8f2-5a6b-7cde-8f12-3456789abcde"}]});
    let filter = r#"{"where":{"_and":[{"distribution_release[status]":{"_eq":"SUCCEEDED"}}]}}"#;
    let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listDistributionReleases",
            "--path",
            "distributionId=dtb_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            "--query",
            "limit=1",
            "--query",
            "cursor=opaque-input",
            "--query",
            &format!("filter={filter}"),
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    let line = requests[0].lines().next().unwrap();
    assert!(line.starts_with("GET /v2/distributions/dtb_0197c8f2-5a6b-7cde-8f12-3456789abcde/releases?"));
    assert!(line.contains("limit=1"));
    assert!(line.contains("cursor=opaque-input"));
    assert!(line.contains("filter="));
    assert!(line.contains("SUCCEEDED"));
}
#[test]
fn distribution_release_reads_preserve_current_access_errors() {
    for (operation, path) in [
        (
            "getDistributionRelease",
            "distributionReleaseId=rel_0197c8f2-5a6b-7cde-8f12-3456789abcde",
        ),
        (
            "listDistributionReleases",
            "distributionId=dtb_0197c8f2-5a6b-7cde-8f12-3456789abcde",
        ),
    ] {
        for (status, code) in [
            ("401 Unauthorized", "disabled_api_key"),
            ("403 Forbidden", "insufficient_permissions"),
            ("404 Not Found", "resource_not_found"),
        ] {
            let body =
                json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"distribution"});
            let stub = Stub::new(vec![response(status, body.clone(), "")]);
            let out = command(
                &stub.origin,
                &["--json", "api", "call", operation, "--path", path],
                None,
            );
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            assert_eq!(
                serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
                body
            );
            assert_eq!(stub.finish().len(), 1);
        }
    }
}

#[test]
fn distribution_release_logs_transmit_level_filter_and_cursor() {
    let body = json!({"object_type":"LIST","has_more":true,"next_cursor":"opaque-next","data":[{"object_type":"DISTRIBUTION_RELEASE_LOG","level":"WARN","content":"Österreich","created_at":"2026-09-30T00:00:00.000Z"}]});
    let filter = r#"{"where":{"_and":[{"distribution_release_log[level]":{"_eq":"WARN"}}]}}"#;
    let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listDistributionReleaseLogs",
            "--path",
            "distributionReleaseId=rel_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            "--query",
            "limit=1",
            "--query",
            "cursor=opaque-input",
            "--query",
            &format!("filter={filter}"),
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    let line = requests[0].lines().next().unwrap();
    assert!(line.starts_with("GET /v2/distribution-releases/rel_0197c8f2-5a6b-7cde-8f12-3456789abcde/logs?"));
    assert!(line.contains("limit=1"));
    assert!(line.contains("cursor=opaque-input"));
    assert!(line.contains("filter="));
    assert!(line.contains("WARN"));
}

#[test]
fn distribution_release_logs_preserve_empty_history_and_access_errors() {
    for (status, body) in [
        (
            "200 OK",
            json!({"object_type":"LIST","has_more":false,"next_cursor":null,"data":[]}),
        ),
        (
            "403 Forbidden",
            json!({"status":403,"code":"insufficient_permissions","request_id":"release-log"}),
        ),
        (
            "404 Not Found",
            json!({"status":404,"code":"resource_not_found","request_id":"release-log"}),
        ),
    ] {
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "listDistributionReleaseLogs",
                "--path",
                "distributionReleaseId=rel_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        if status == "200 OK" {
            assert!(out.status.success());
            assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        } else {
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            assert_eq!(
                serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
                body
            );
        }
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn distribution_release_deletion_forwards_condition_and_accepts_no_content() {
    let stub = Stub::new(vec![
        "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".into(),
    ]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--if-match",
            "\"release-version\"",
            "api",
            "call",
            "deleteDistributionRelease",
            "--path",
            "distributionReleaseId=rel_0197c8f2-5a6b-7cde-8f12-3456789abcde",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty());
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].starts_with("DELETE /v2/distribution-releases/rel_0197c8f2-5a6b-7cde-8f12-3456789abcde ")
    );
    assert!(
        requests[0]
            .to_lowercase()
            .contains("if-match: \"release-version\"")
    );
}

#[test]
fn distribution_release_deletion_preserves_stale_and_current_access_problems() {
    for (status, code) in [
        ("412 Precondition Failed", "precondition_failed"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let body =
            json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"release-delete"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "deleteDistributionRelease",
                "--path",
                "distributionReleaseId=rel_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--if-match",
                "\"old-release\"",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

fn release_download(origin: &str, destination: &std::path::Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec![
        "--json",
        "api",
        "download-distribution-release",
        "rel_0197c8f2-5a6b-7cde-8f12-3456789abcde",
        "--output",
        destination.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    command(origin, &args, None)
}
fn release_download_metadata(url: Value) -> Value {
    json!({"object_type":"DISTRIBUTION_RELEASE_DOWNLOAD", "id":"rel_0197c8f2-5a6b-7cde-8f12-3456789abcde", "download_url":url, "download_url_expires_at":"2026-09-30T22:00:00.000Z"})
}
#[test]
fn release_download_refreshes_metadata_and_keeps_credentials_off_artifact_requests() {
    let fixture = UploadFixture::new("marker", b"preserve");
    let destination = fixture.0.with_file_name("release.zip");
    let bytes = [0u8, 255, b'P', b'K', 10];
    let artifact = Stub::bytes(vec![artifact_response(&bytes)]);
    let api = Stub::new(vec![response(
        "200 OK",
        release_download_metadata(json!(format!(
            "{}/release.zip?token=secret-link",
            artifact.origin
        ))),
        "X-Request-ID: release-req\r\n",
    )]);
    let out = release_download(&api.origin, &destination, &["--include-headers"]);
    let body = success(&out);
    assert_eq!(
        body["distribution_release_id"],
        "rel_0197c8f2-5a6b-7cde-8f12-3456789abcde"
    );
    assert_eq!(body["bytes_written"], bytes.len());
    assert_eq!(std::fs::read(&destination).unwrap(), bytes);
    assert!(String::from_utf8_lossy(&out.stderr).contains("release-req"));
    let request = &api.finish()[0];
    assert!(
        request
            .starts_with("GET /v2/distribution-releases/rel_0197c8f2-5a6b-7cde-8f12-3456789abcde/download ")
    );
    assert!(
        request
            .to_lowercase()
            .contains("authorization: bearer test-secret-do-not-print")
    );
    let raw = &artifact.finish()[0];
    for header in [
        "authorization:",
        "cookie:",
        "if-match:",
        "idempotency-key:",
        "referer:",
    ] {
        assert!(!raw.to_lowercase().contains(header));
    }
    no_download_leaks(&out);
    assert_no_staging(&fixture);
}
#[test]
fn release_download_rejects_unavailable_or_mismatched_metadata_before_storage() {
    let trap = Stub::new(vec![]);
    let mut wrong_kind = release_download_metadata(json!(format!("{}/archive.zip", trap.origin)));
    wrong_kind["object_type"] = json!("EXPORT");
    let mut wrong_id = release_download_metadata(json!(format!("{}/archive.zip", trap.origin)));
    wrong_id["id"] = json!("rel_other");
    for (metadata, expected_error) in [
        (
            release_download_metadata(Value::Null),
            "No completed archive is available; inspect the distribution release status before downloading",
        ),
        (
            wrong_kind,
            "Download metadata does not match the requested distribution release",
        ),
        (
            wrong_id,
            "Download metadata does not match the requested distribution release",
        ),
        (
            release_download_metadata(json!("http://untrusted.invalid/archive.zip")),
            "Artifact download URL must use HTTPS (or HTTP loopback for local testing), without embedded credentials or a fragment",
        ),
    ] {
        let fixture = UploadFixture::new("marker", b"preserve");
        let destination = fixture.0.with_file_name("release.zip");
        let api = Stub::new(vec![response("200 OK", metadata, "")]);
        let out = release_download(&api.origin, &destination, &[]);
        assert_download_metadata_error(&out, expected_error);
        assert!(!destination.exists());
        assert_eq!(api.finish().len(), 1);
        assert_no_staging(&fixture);
    }
    trap.finish();
}

#[test]
fn release_download_preserves_permission_problems_without_storage_or_partial_files() {
    for (status, code) in [
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let fixture = UploadFixture::new("marker", b"preserve");
        let destination = fixture.0.with_file_name("release.zip");
        let body = json!({"code":code,"status":status[..3].parse::<u16>().unwrap(),"detail":"Unavailable"});
        let api = Stub::new(vec![response(status, body.clone(), "")]);
        let out = release_download(&api.origin, &destination, &[]);
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert!(!destination.exists());
        assert_eq!(api.finish().len(), 1);
        assert_no_staging(&fixture);
    }
}
#[test]
fn release_download_reuses_redirect_expiry_and_size_safeguards() {
    let trap = Stub::new(vec![]);
    for status in ["302 Found", "403 Forbidden", "206 Partial Content", "200 OK"] {
        let fixture = UploadFixture::new("marker", b"preserve");
        let destination = fixture.0.with_file_name("release.zip");
        let artifact = Stub::new(vec![response(
            status,
            json!({"detail":"provider-private-body secret-link"}),
            &format!("Location: {}/?secret-link\r\n", trap.origin),
        )]);
        let api = Stub::new(vec![response(
            "200 OK",
            release_download_metadata(json!(format!(
                "{}/archive.zip?token=secret-link",
                artifact.origin
            ))),
            "",
        )]);
        let out = release_download(
            &api.origin,
            &destination,
            &[
                "--max-bytes",
                if status.starts_with("200") { "1" } else { "1024" },
            ],
        );
        let expected = if status.starts_with("200") {
            "max-bytes".into()
        } else {
            format!("HTTP {}", &status[..3])
        };
        assert_error_contains(&out, &expected);
        assert!(!destination.exists());
        assert_eq!(api.finish().len(), 1);
        assert_eq!(artifact.finish().len(), 1);
        no_download_leaks(&out);
        assert_no_staging(&fixture);
    }
    trap.finish();
}

#[test]
fn release_download_preserves_existing_outputs_and_rejects_write_flags() {
    let fixture = UploadFixture::new("release.zip", b"preserve");
    let out = release_download("http://127.0.0.1:1", &fixture.0, &[]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(std::fs::read(&fixture.0).unwrap(), b"preserve");
    for flag in ["--if-match", "--idempotency-key", "--file"] {
        let out = release_download(
            "http://127.0.0.1:1",
            &fixture.0.with_file_name("new.zip"),
            &[flag, "value"],
        );
        assert_eq!(out.status.code(), Some(2));
    }
    assert_no_staging(&fixture);
}

#[test]
fn release_download_rejects_unexpected_success_status_before_storage() {
    let trap = Stub::new(vec![]);
    for status in ["201 Created", "202 Accepted"] {
        let fixture = UploadFixture::new("marker", b"preserve");
        let destination = fixture.0.with_file_name("release.zip");
        let api = Stub::new(vec![response(
            status,
            release_download_metadata(json!(format!("{}/archive.zip", trap.origin))),
            "",
        )]);
        let out = release_download(&api.origin, &destination, &[]);
        assert_download_metadata_error(&out, "Expected HTTP 200 download metadata");
        assert!(!destination.exists());
        assert_eq!(api.finish().len(), 1);
        assert_no_staging(&fixture);
    }
    trap.finish();
}

#[test]
fn distribution_deletion_forwards_condition_and_accepts_no_content() {
    let stub = Stub::new(vec![
        "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".into(),
    ]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--if-match",
            "\"release-version\"",
            "api",
            "call",
            "deleteDistribution",
            "--path",
            "distributionId=dtb_0197c8f2-5a6b-7cde-8f12-3456789abcde",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stdout.is_empty());
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("DELETE /v2/distributions/dtb_0197c8f2-5a6b-7cde-8f12-3456789abcde "));
    assert!(
        requests[0]
            .to_lowercase()
            .contains("if-match: \"release-version\"")
    );
}

#[test]
fn distribution_deletion_preserves_stale_and_current_access_problems() {
    for (status, code) in [
        ("412 Precondition Failed", "precondition_failed"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let body =
            json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"release-delete"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "deleteDistribution",
                "--path",
                "distributionId=dtb_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--if-match",
                "\"old-release\"",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn cdn_key_inventory_preserves_public_metadata_filters_and_cursors() {
    let id = "cdn_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let project = "pr_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let metadata = json!({"object_type":"CDN_KEY","id":id,"project_id":project,"name":"Production","last_used_at":null,"created_at":"2026-09-30T00:00:00.000Z","updated_at":"2026-09-30T00:00:00.000Z"});
    let page =
        json!({"object_type":"LIST","has_more":true,"next_cursor":"next-key","data":[metadata.clone()]});
    let filter = r#"{"where":{"_and":[{"cdn_key[name]":{"_eq":"Production"}}]}}"#;
    let stub = Stub::new(vec![
        response("200 OK", page.clone(), ""),
        response("200 OK", metadata.clone(), "ETag: \"key-state\"\r\n"),
    ]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listProjectCdnKeys",
            "--path",
            &format!("projectId={project}"),
            "--query",
            &format!("filter={filter}"),
            "--query",
            "cursor=previous",
            "--query",
            "limit=1",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), page);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "getCdnKey",
            "--path",
            &format!("cdnKeyId={id}"),
            "--include-headers",
        ],
        None,
    );
    assert!(out.status.success());
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), metadata);
    assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"key-state\""));
    let requests = stub.finish();
    assert!(requests[0].starts_with(&format!("GET /v2/projects/{project}/cdn-keys?")));
    let target = reqwest::Url::parse(&format!(
        "http://localhost{}",
        requests[0].split_whitespace().nth(1).unwrap()
    ))
    .unwrap();
    let query: std::collections::BTreeMap<_, _> = target.query_pairs().into_owned().collect();
    assert_eq!(query.len(), 3);
    assert_eq!(query.get("cursor").map(String::as_str), Some("previous"));
    assert_eq!(query.get("limit").map(String::as_str), Some("1"));
    assert_eq!(query.get("filter").map(String::as_str), Some(filter));
    assert!(requests[1].starts_with(&format!("GET /v2/cdn-keys/{id} HTTP/1.1")));
}

#[test]
fn cdn_key_inventory_preserves_current_permission_and_visibility_errors() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        for (operation, path) in [
            ("getCdnKey", "cdnKeyId=cdn_0197c8f2-5a6b-7cde-8f12-3456789abcde"),
            (
                "listProjectCdnKeys",
                "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ),
        ] {
            let body =
                json!({"code":code,"status":status[..3].parse::<u16>().unwrap(),"detail":"Unavailable"});
            let stub = Stub::new(vec![response(status, body.clone(), "")]);
            let out = command(
                &stub.origin,
                &["--json", "api", "call", operation, "--path", path],
                None,
            );
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            assert_eq!(
                serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
                body
            );
            assert_eq!(stub.finish().len(), 1);
        }
    }
}

#[test]
fn cdn_key_deletion_forwards_condition_and_accepts_no_content() {
    for condition in [None, Some("\"key-version\""), Some("*")] {
        let stub = Stub::new(vec![
            "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".into(),
        ]);
        let mut args = vec![
            "--json",
            "api",
            "call",
            "deleteCdnKey",
            "--path",
            "cdnKeyId=cdn_0197c8f2-5a6b-7cde-8f12-3456789abcde",
        ];
        if let Some(value) = condition {
            args.extend(["--if-match", value]);
        }
        let out = command(&stub.origin, &args, None);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(out.stdout.is_empty());
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("DELETE /v2/cdn-keys/cdn_0197c8f2-5a6b-7cde-8f12-3456789abcde "));
        let (headers, body) = requests[0].split_once("\r\n\r\n").unwrap();
        assert!(body.is_empty());
        match condition {
            Some(value) => assert!(headers.to_lowercase().contains(&format!("if-match: {value}"))),
            None => assert!(!headers.to_lowercase().contains("if-match:")),
        }
    }
}

#[test]
fn cdn_key_deletion_preserves_stale_and_current_access_problems() {
    for (status, code) in [
        ("412 Precondition Failed", "precondition_failed"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let body =
            json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"key-delete"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "deleteCdnKey",
                "--path",
                "cdnKeyId=cdn_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--if-match",
                "\"old-key\"",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn release_key_snapshots_preserve_unknown_zero_and_bundled_entry_counts() {
    for counts in [
        Value::Null,
        json!({"total":0,"entries":[]}),
        json!({"total":500,"entries":[{"file_path":"bundled.json","language_codes":["en","de"],"key_count":500},{"file_path":"empty.json","language_codes":["fr"],"key_count":0}]}),
    ] {
        let body = json!({"object_type":"DISTRIBUTION_RELEASE","id":"rel_0197c8f2-5a6b-7cde-8f12-3456789abcde","key_counts":counts});
        let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "getDistributionRelease",
                "--path",
                "distributionReleaseId=rel_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn entry_statistics_preserve_nullable_sizes_and_transmit_integer_filters() {
    for (operation, path, filter) in [
        (
            "listExportEntries",
            "exportId=reg_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            r#"{"where":{"_and":[{"export_entry[key_count]":{"_eq":0}},{"export_entry[file_size_bytes]":{"_gt":0}}]}}"#,
        ),
        (
            "listImportEntries",
            "importId=rin_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            r#"{"where":{"_and":[{"import_entry[file_size_bytes]":{"_gt":0}}]}}"#,
        ),
    ] {
        let entries = if operation == "listExportEntries" {
            json!([{"key_count":0,"file_size_bytes":2048},{"key_count":null,"file_size_bytes":null}])
        } else {
            json!([{"file_size_bytes":2048},{"file_size_bytes":null}])
        };
        let body = json!({"object_type":"LIST","has_more":false,"next_cursor":null,"data":entries});
        let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                operation,
                "--path",
                path,
                "--query",
                &format!("filter={filter}"),
            ],
            None,
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        let query = reqwest::Url::parse(&format!(
            "http://localhost{}",
            requests[0].split_whitespace().nth(1).unwrap()
        ))
        .unwrap();
        assert!(
            query
                .query_pairs()
                .any(|(name, value)| name == "filter" && value == filter)
        );
    }
}

#[test]
fn bundled_statistics_schema_preserves_required_nullable_and_typed_values() {
    let spec: Value = serde_json::from_str(include_str!("../contract/v2.json")).unwrap();
    let schemas = &spec["components"]["schemas"];
    let release = &schemas["DistributionRelease"];
    assert!(
        release["required"]
            .as_array()
            .unwrap()
            .contains(&json!("key_counts"))
    );
    let counts = &release["properties"]["key_counts"];
    assert_eq!(counts["nullable"], true);
    assert_eq!(
        counts["oneOf"][0]["$ref"],
        "#/components/schemas/DistributionReleaseKeyCountSnapshot"
    );
    let snapshot = &schemas["DistributionReleaseKeyCountSnapshot"];
    for field in ["total", "entries"] {
        assert!(snapshot["required"].as_array().unwrap().contains(&json!(field)));
    }
    assert_eq!(snapshot["properties"]["total"]["type"], "integer");
    assert_eq!(snapshot["properties"]["total"]["minimum"], 0);
    assert_eq!(snapshot["properties"]["entries"]["type"], "array");
    assert_eq!(
        snapshot["properties"]["entries"]["items"]["$ref"],
        "#/components/schemas/DistributionReleaseFileKeyCount"
    );
    let entry = &schemas["DistributionReleaseFileKeyCount"];
    for field in ["file_path", "language_codes", "key_count"] {
        assert!(entry["required"].as_array().unwrap().contains(&json!(field)));
    }
    assert_eq!(entry["properties"]["file_path"]["type"], "string");
    assert_eq!(entry["properties"]["language_codes"]["type"], "array");
    assert_eq!(entry["properties"]["language_codes"]["items"]["type"], "string");
    assert_eq!(entry["properties"]["key_count"]["type"], "integer");
    assert_eq!(entry["properties"]["key_count"]["format"], "int32");
    assert_eq!(entry["properties"]["key_count"]["minimum"], 0);
    for (resource, field, format) in [
        ("ExportEntry", "export_entry[key_count]", "int32"),
        ("ExportEntry", "export_entry[file_size_bytes]", "int64"),
        ("ImportEntry", "import_entry[file_size_bytes]", "int64"),
    ] {
        let operators = &schemas[format!("{resource}FilterClause")]["properties"][field]["properties"];
        for operator in ["_eq", "_ne", "_gt", "_gte", "_lt", "_lte"] {
            assert_eq!(operators[operator]["type"], "integer");
            assert_eq!(operators[operator]["format"], format);
        }
        for operator in ["_in", "_nin", "_within"] {
            assert_eq!(operators[operator]["type"], "array");
            assert_eq!(operators[operator]["items"]["type"], "integer");
            assert_eq!(operators[operator]["items"]["format"], format);
        }
        assert_eq!(operators["_within"]["minItems"], 2);
        assert_eq!(operators["_within"]["maxItems"], 2);
        for operator in ["_null", "_nnull"] {
            assert_eq!(operators[operator]["type"], "boolean");
            assert_eq!(operators[operator]["enum"], json!([true]));
        }
        // No `sort` property at all means the entries cannot be sorted by any field.
        let sortable = schemas[format!("{resource}Filter")]["properties"]["sort"]["items"]["enum"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert!(
            !sortable
                .iter()
                .any(|value| { value.as_str().unwrap().trim_start_matches(['+', '-']) == field })
        );
    }
    for (schema, field, format) in [
        ("ExportEntry", "key_count", "int32"),
        ("ExportEntry", "file_size_bytes", "int64"),
        ("ImportEntry", "file_size_bytes", "int64"),
    ] {
        assert!(
            schemas[schema]["required"]
                .as_array()
                .unwrap()
                .contains(&json!(field))
        );
        let property = &schemas[schema]["properties"][field];
        assert_eq!(property["nullable"], true);
        assert_eq!(property["type"], "integer");
        assert_eq!(property["format"], format);
        assert_eq!(property["minimum"], 0);
    }
}

#[test]
fn backup_settings_deletion_forwards_condition_and_accepts_no_content() {
    for condition in [None, Some("\"key-version\""), Some("*")] {
        let stub = Stub::new(vec![
            "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".into(),
        ]);
        let mut args = vec![
            "--json",
            "api",
            "call",
            "deleteBackupSettings",
            "--path",
            "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
        ];
        if let Some(value) = condition {
            args.extend(["--if-match", value]);
        }
        let out = command(&stub.origin, &args, None);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(out.stdout.is_empty());
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .starts_with("DELETE /v2/projects/pr_0197c8f2-5a6b-7cde-8f12-3456789abcde/backup-settings ")
        );
        let (headers, body) = requests[0].split_once("\r\n\r\n").unwrap();
        assert!(body.is_empty());
        match condition {
            Some(value) => assert!(headers.to_lowercase().contains(&format!("if-match: {value}"))),
            None => assert!(!headers.to_lowercase().contains("if-match:")),
        }
    }
}

#[test]
fn backup_settings_deletion_preserves_stale_and_current_access_problems() {
    for (status, code) in [
        ("412 Precondition Failed", "precondition_failed"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let body =
            json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"key-delete"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "deleteBackupSettings",
                "--path",
                "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--if-match",
                "\"old-key\"",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn bodyless_named_operations_reject_fields_files_and_stdin_before_io() {
    for (operation, parameter) in [
        ("deleteBackupSettings", "projectId=pr_123"),
        ("deleteProjectGitIntegration", "projectId=pr_123"),
        ("getBackupSettings", "projectId=pr_123"),
        ("deleteStack", "stackId=stack_123"),
        ("resendInvitation", "invitationId=inv_123"),
        ("activateDistributionRelease", "distributionReleaseId=rel_123"),
        ("closeTask", "taskId=task_123"),
        ("skipTask", "taskId=task_123"),
        ("getTaskCounts", "taskId=task_123"),
        ("getTaskDetails", "taskId=task_123"),
    ] {
        for flags in [
            ["--field", "unexpected=true"],
            ["--file", "missing.json"],
            ["--file", "-"],
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let out = command(
                &origin,
                &["api", "call", operation, "--path", parameter, flags[0], flags[1]],
                Some("not JSON"),
            );
            assert_eq!(out.status.code(), Some(2));
            assert!(out.stdout.is_empty());
            assert!(String::from_utf8_lossy(&out.stderr).contains("does not accept a request body"));
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }
}

#[test]
fn release_activation_forwards_conditions_idempotency_and_complete_metadata() {
    let id = "rel_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let metadata = json!({
        "object_type":"DISTRIBUTION_RELEASE", "id":id,
        "distribution_id":"dtb_0197c8f2-5a6b-7cde-8f12-3456789abcde",
        "project_id":"pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
        "status":"SUCCEEDED", "activated_at":"2026-10-01T04:00:00.123456Z",
        "name":"October β", "branch_name":"main", "language_codes":["en","de"],
        "archive_size_kib":12, "file_count":2,
        "created_at":"2026-10-01T03:00:00.123456Z", "updated_at":"2026-10-01T04:00:00.123456Z",
        "key_counts":{"total":5,"entries":[
            {"file_path":"en.json","language_codes":["en"],"key_count":5},
            {"file_path":"de.json","language_codes":["de"],"key_count":0}
        ]}
    });
    let contract: Value = serde_json::from_str(include_str!("../contract/v2.json")).unwrap();
    for field in contract["components"]["schemas"]["DistributionRelease"]["required"]
        .as_array()
        .unwrap()
    {
        assert!(
            metadata.get(field.as_str().unwrap()).is_some(),
            "missing required field: {field}"
        );
    }
    for condition in [None, Some("*"), Some("\"release-version\"")] {
        let stub = Stub::new(vec![response("200 OK", metadata.clone(), "")]);
        let path = format!("distributionReleaseId={id}");
        let mut args = vec![
            "--json",
            "api",
            "call",
            "activateDistributionRelease",
            "--path",
            &path,
            "--idempotency-key",
            "activate-once",
        ];
        if let Some(value) = condition {
            args.extend(["--if-match", value]);
        }
        let out = command(&stub.origin, &args, None);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), metadata);
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("POST /v2/distribution-releases/{id}/activate HTTP/1.1")));
        let (headers, body) = requests[0].split_once("\r\n\r\n").unwrap();
        assert!(body.is_empty());
        assert!(headers.to_lowercase().contains("idempotency-key: activate-once"));
        match condition {
            Some(value) => assert!(headers.to_lowercase().contains(&format!("if-match: {value}"))),
            None => assert!(!headers.to_lowercase().contains("if-match:")),
        }
    }
}

#[test]
fn release_activation_preserves_count_state_and_access_problems_without_retry() {
    for (status, code) in [
        ("409 Conflict", "distribution_release_not_ready"),
        ("409 Conflict", "distribution_release_counts_unavailable"),
        ("409 Conflict", "distribution_release_empty"),
        ("412 Precondition Failed", "precondition_failed"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let body = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"release-activation"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "activateDistributionRelease",
                "--path",
                "distributionReleaseId=rel_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn task_discovery_preserves_complete_metadata_pages_filters_and_validators() {
    let id = "task_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let metadata = json!({
        "object_type":"TASK", "id":id,
        "workspace_id":"space_0197c8f2-5a6b-7cde-8f12-3456789abcde", "project_id":null,
        "category":"MEMBERS", "type":"INVITATION_RECEIVED", "priority":"NORMAL", "status":"OPEN",
        "is_user_closable":false, "is_skippable":false,
        "last_occurrence_at":"2026-10-01T05:00:00.123456Z", "closed_at":null,
        "created_at":"2026-10-01T04:00:00.123456Z", "updated_at":"2026-10-01T05:00:00.123456Z"
    });
    let contract: Value = serde_json::from_str(include_str!("../contract/v2.json")).unwrap();
    let schema = &contract["components"]["schemas"]["Task"];
    for field in schema["required"].as_array().unwrap() {
        assert!(
            metadata.get(field.as_str().unwrap()).is_some(),
            "missing required field: {field}"
        );
    }
    assert_eq!(
        schema["properties"].as_object().unwrap().len(),
        metadata.as_object().unwrap().len()
    );
    for (name, field, expected) in [
        (
            "TaskCategory",
            "category",
            json!([
                "ONBOARDING",
                "SUBSCRIPTION",
                "MANAGEMENT",
                "MEMBERS",
                "EDITING",
                "REVIEWING"
            ]),
        ),
        ("TaskPriority", "priority", json!(["NORMAL", "ATTENTION"])),
        ("TaskStatus", "status", json!(["OPEN", "DONE", "SKIPPED"])),
        (
            "TaskType",
            "type",
            json!([
                "EXPLORE_EXAMPLE_PROJECT",
                "SETUP_CONTEXT",
                "CREATE_STYLE_GUIDE",
                "CREATE_GLOSSARY",
                "CONFIGURE_LINA",
                "CREATE_FIRST_PROJECT",
                "INVITE_MEMBERS",
                "CREATE_STACK",
                "PAYMENT_FAILED",
                "LIMITS_REACHED",
                "BUDGET_REACHED",
                "INTEGRATION_ERROR",
                "IMPORT_ISSUES",
                "INVITATION_RECEIVED",
                "INVITATION_PENDING",
                "RESOLVE_BLOCKED_SEGMENTS",
                "TRANSLATE_SEGMENTS",
                "REVIEW_SEGMENTS"
            ]),
        ),
    ] {
        assert_eq!(
            contract["components"]["schemas"][name]["enum"], expected,
            "{name}"
        );
        if field == "priority" {
            assert!(
                contract["components"]["schemas"]["TaskFilterClause"]["properties"]
                    .get("task[priority]")
                    .is_none()
            );
            continue;
        }
        let filter = &contract["components"]["schemas"]["TaskFilterClause"]["properties"]
            [format!("task[{field}]")]["properties"];
        for operator in ["_eq", "_ne"] {
            assert_eq!(filter[operator]["enum"], expected, "{field} {operator}");
        }
        for operator in ["_in", "_nin"] {
            assert_eq!(filter[operator]["items"]["enum"], expected, "{field} {operator}");
        }
    }
    let cursor = "next-task+/= opaque";
    let page = json!({"object_type":"LIST","has_more":true,"next_cursor":cursor,"data":[metadata.clone()]});
    let last = json!({"object_type":"LIST","has_more":false,"next_cursor":null,"data":[]});
    let filter =
        r#"{"where":{"_and":[{"task[status]":{"_eq":"OPEN"}}]},"sort":["-task[last_occurrence_at]"]}"#;
    let stub = Stub::new(vec![
        response("200 OK", page.clone(), ""),
        response("200 OK", last.clone(), ""),
        response("200 OK", metadata.clone(), "ETag: \"task-state\"\r\n"),
    ]);
    for (args, expected) in [
        (
            vec![
                "--json".to_owned(),
                "api".into(),
                "call".into(),
                "listTasks".into(),
                "--query".into(),
                format!("filter={filter}"),
                "--query".into(),
                "limit=1".into(),
            ],
            page,
        ),
        (
            vec![
                "--json".to_owned(),
                "api".into(),
                "call".into(),
                "listTasks".into(),
                "--query".into(),
                format!("filter={filter}"),
                "--query".into(),
                "limit=1".into(),
                "--query".into(),
                format!("cursor={cursor}"),
            ],
            last,
        ),
        (
            vec![
                "--json".to_owned(),
                "api".into(),
                "call".into(),
                "getTask".into(),
                "--path".into(),
                format!("taskId={id}"),
                "--include-headers".into(),
            ],
            metadata,
        ),
    ] {
        let args: Vec<_> = args.iter().map(String::as_str).collect();
        let out = command(&stub.origin, &args, None);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), expected);
        if args.contains(&"getTask") {
            assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"task-state\""));
        }
    }
    let requests = stub.finish();
    assert_eq!(requests.len(), 3);
    for (index, request) in requests.iter().take(2).enumerate() {
        assert!(request.starts_with("GET /v2/tasks?"));
        let url = reqwest::Url::parse(&format!(
            "http://localhost{}",
            request.split_whitespace().nth(1).unwrap()
        ))
        .unwrap();
        let query: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query.len(), if index == 0 { 2 } else { 3 });
        assert_eq!(query.get("filter").map(String::as_str), Some(filter));
        assert_eq!(query.get("limit").map(String::as_str), Some("1"));
        assert_eq!(
            query.get("cursor").map(String::as_str),
            if index == 0 { None } else { Some(cursor) }
        );
    }
    assert!(requests[2].starts_with(&format!("GET /v2/tasks/{id} HTTP/1.1")));
}

#[test]
fn task_discovery_preserves_current_audience_and_input_problems_without_retry() {
    let id = "task_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    for (operation, status, code, query) in [
        ("listTasks", "401 Unauthorized", "disabled_api_key", None),
        ("getTask", "401 Unauthorized", "disabled_api_key", None),
        ("listTasks", "403 Forbidden", "insufficient_permissions", None),
        ("getTask", "403 Forbidden", "insufficient_permissions", None),
        ("getTask", "404 Not Found", "resource_not_found", None),
        (
            "listTasks",
            "400 Bad Request",
            "invalid_cursor",
            Some(("cursor", "broken")),
        ),
        (
            "listTasks",
            "400 Bad Request",
            "invalid_filter",
            Some((
                "filter",
                r#"{"where":{"_and":[{"task[status]":{"_eq":"MISSING"}}]}}"#,
            )),
        ),
        (
            "listTasks",
            "400 Bad Request",
            "unknown_parameter",
            Some(("unused", "1")),
        ),
        (
            "getTask",
            "400 Bad Request",
            "unknown_parameter",
            Some(("unused", "1")),
        ),
    ] {
        let body = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"task-discovery","detail":"Unavailable"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let path = format!("taskId={id}");
        let mut args = vec!["--json", "api", "call", operation];
        if operation == "getTask" {
            args.extend(["--path", &path]);
        }
        let query_arg = query.map(|(name, value)| format!("{name}={value}"));
        if let Some(value) = &query_arg {
            args.extend(["--query", value]);
        }
        let out = command(&stub.origin, &args, None);
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        let target = reqwest::Url::parse(&format!(
            "http://localhost{}",
            requests[0].split_whitespace().nth(1).unwrap()
        ))
        .unwrap();
        assert_eq!(
            target.path(),
            if operation == "getTask" {
                format!("/v2/tasks/{id}")
            } else {
                "/v2/tasks".into()
            }
        );
        let actual: Vec<_> = target.query_pairs().into_owned().collect();
        let expected: Vec<_> = query
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        assert_eq!(actual, expected);
    }
}

#[test]
fn task_actions_preserve_bodyless_conditions_replay_keys_and_complete_metadata() {
    let id = "task_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let contract: Value = serde_json::from_str(include_str!("../contract/v2.json")).unwrap();
    for (operation, action, status) in [("closeTask", "close", "DONE"), ("skipTask", "skip", "SKIPPED")] {
        let metadata = json!({
            "object_type":"TASK", "id":id,
            "workspace_id":"space_0197c8f2-5a6b-7cde-8f12-3456789abcde", "project_id":null,
            "category":"ONBOARDING", "type":"EXPLORE_EXAMPLE_PROJECT", "priority":"NORMAL", "status":status,
            "is_user_closable":true, "is_skippable":true,
            "last_occurrence_at":"2026-10-01T05:00:00.123Z", "closed_at":"2026-10-01T06:00:00.123Z",
            "created_at":"2026-10-01T04:00:00.123Z", "updated_at":"2026-10-01T06:00:00.123Z"
        });
        let schema = &contract["components"]["schemas"]["Task"];
        for field in schema["required"].as_array().unwrap() {
            assert!(
                metadata.get(field.as_str().unwrap()).is_some(),
                "missing field: {field}"
            );
        }
        assert_eq!(
            schema["properties"].as_object().unwrap().len(),
            metadata.as_object().unwrap().len()
        );
        for (field, component) in [
            ("category", "TaskCategory"),
            ("type", "TaskType"),
            ("priority", "TaskPriority"),
            ("status", "TaskStatus"),
        ] {
            assert!(
                contract["components"]["schemas"][component]["enum"]
                    .as_array()
                    .unwrap()
                    .contains(&metadata[field])
            );
        }
        assert!(
            contract["paths"][format!("/v2/tasks/{{taskId}}/{action}")]["post"]
                .get("requestBody")
                .is_none()
        );
        let path = format!("taskId={id}");
        for condition in [None, Some("*"), Some("\"task-before\"")] {
            let stub = Stub::new(vec![response(
                "200 OK",
                metadata.clone(),
                "ETag: \"task-after\"\r\n",
            )]);
            let mut args = vec![
                "--json",
                "--include-headers",
                "api",
                "call",
                operation,
                "--path",
                &path,
                "--idempotency-key",
                "task-once",
            ];
            if let Some(value) = condition {
                args.extend(["--if-match", value]);
            }
            let out = command(&stub.origin, &args, None);
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            let result: Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(result, metadata);
            assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"task-after\""));
            let requests = stub.finish();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].starts_with(&format!("POST /v2/tasks/{id}/{action} HTTP/1.1")));
            let (headers, body) = requests[0].split_once("\r\n\r\n").unwrap();
            assert!(body.is_empty());
            let headers = headers.to_lowercase();
            assert!(headers.contains("idempotency-key: task-once"));
            match condition {
                Some(value) => assert!(headers.contains(&format!("if-match: {value}"))),
                None => assert!(!headers.contains("if-match:")),
            }
        }
    }
}

#[test]
fn task_action_problems_preserve_policy_scope_and_precondition_without_retry() {
    for (operation, action, policy) in [
        ("closeTask", "close", "task_not_closable"),
        ("skipTask", "skip", "task_not_skippable"),
    ] {
        for (status, code) in [
            ("401 Unauthorized", "invalid_api_key"),
            ("403 Forbidden", "insufficient_permissions"),
            ("404 Not Found", "resource_not_found"),
            ("409 Conflict", "task_not_open"),
            ("409 Conflict", policy),
            ("412 Precondition Failed", "precondition_failed"),
        ] {
            let problem = json!({"status":status[..3].parse::<u16>().unwrap(), "code":code, "request_id":"task-action"});
            let stub = Stub::new(vec![response(status, problem.clone(), "")]);
            let out = command(
                &stub.origin,
                &[
                    "--json",
                    "api",
                    "call",
                    operation,
                    "--path",
                    "taskId=task_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                    "--if-match",
                    "\"task-before\"",
                    "--idempotency-key",
                    "task-once",
                ],
                None,
            );
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            assert_eq!(
                serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
                problem
            );
            let requests = stub.finish();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].starts_with(&format!(
                "POST /v2/tasks/task_0197c8f2-5a6b-7cde-8f12-3456789abcde/{action} HTTP/1.1"
            )));
        }
    }
}

#[test]
fn task_counts_preserve_null_zero_int64_freshness_and_validators() {
    let spec: Value = serde_json::from_str(include_str!("../contract/v2.json")).unwrap();
    let schema = &spec["components"]["schemas"]["TaskCounts"];
    assert!(spec["paths"]["/v2/tasks/{taskId}/counts"]["get"]["responses"]["200"]["headers"]["ETag"]["description"].as_str().unwrap().contains("not accepted by task actions"));
    for field in [
        "object_type",
        "id",
        "segment_count",
        "source_word_count",
        "counts_updated_at",
    ] {
        assert!(schema["required"].as_array().unwrap().contains(&json!(field)));
    }
    for field in ["segment_count", "source_word_count"] {
        assert_eq!(schema["properties"][field]["type"], "integer");
        assert_eq!(schema["properties"][field]["format"], "int64");
        assert_eq!(schema["properties"][field]["nullable"], true);
        assert_eq!(schema["properties"][field]["minimum"], 0);
    }
    assert_eq!(schema["properties"]["counts_updated_at"]["nullable"], true);
    assert_eq!(schema["properties"]["counts_updated_at"]["type"], "string");
    assert_eq!(schema["properties"]["counts_updated_at"]["format"], "date-time");
    for (segments, words, updated) in [
        (Value::Null, Value::Null, Value::Null),
        (json!(0), json!(0), json!("2026-10-01T07:00:00.000Z")),
        (
            json!(2147483656_i64),
            json!(2147483660_i64),
            json!("2026-10-01T06:00:00.000Z"),
        ),
    ] {
        let body = json!({"object_type":"TASK_COUNTS","id":"task_0197c8f2-5a6b-7cde-8f12-3456789abcde","segment_count":segments,"source_word_count":words,"counts_updated_at":updated});
        let stub = Stub::new(vec![response(
            "200 OK",
            body.clone(),
            "ETag: \"task-counts\"\r\n",
        )]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "getTaskCounts",
                "--path",
                "taskId=task_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--include-headers",
            ],
            None,
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .any(|line| line.eq_ignore_ascii_case("etag: \"task-counts\""))
        );
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .starts_with("GET /v2/tasks/task_0197c8f2-5a6b-7cde-8f12-3456789abcde/counts HTTP/1.1")
        );
    }
}

#[test]
fn task_counts_standard_input_and_access_problems_are_not_retried() {
    for (status, code) in [
        ("400 Bad Request", "unknown_parameter"),
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
    ] {
        let body = json!({"type":"about:blank","title":"Task counts unavailable","status":status[..3].parse::<u16>().unwrap(),"code":code});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let args = if code == "unknown_parameter" {
            vec![
                "--json",
                "api",
                "call",
                "getTaskCounts",
                "--path",
                "taskId=task_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--query",
                "unused=1",
            ]
        } else {
            vec![
                "--json",
                "api",
                "call",
                "getTaskCounts",
                "--path",
                "taskId=task_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ]
        };
        let out = command(&stub.origin, &args, None);
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn task_details_preserve_typed_nullable_context_and_validators() {
    let id = "task_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let contract: Value = serde_json::from_str(include_str!("../contract/v2.json")).unwrap();
    let schema = &contract["components"]["schemas"]["TaskDetails"];
    assert!(contract["paths"]["/v2/tasks/{taskId}/details"]["get"]["responses"]["200"]["headers"]["ETag"]["description"].as_str().unwrap().contains("not accepted by task actions"));
    let base = json!({"object_type":"TASK_DETAILS","id":id,"type":"EXPLORE_EXAMPLE_PROJECT","segment":null,"import":null,"integration":null,"invitation":null});
    assert_eq!(
        schema["properties"].as_object().unwrap().len(),
        base.as_object().unwrap().len()
    );
    assert_eq!(
        schema["required"],
        json!([
            "id",
            "import",
            "integration",
            "invitation",
            "object_type",
            "segment",
            "type"
        ])
    );
    for (field, component) in [
        ("segment", "TaskSegmentDetails"),
        ("import", "TaskImportDetails"),
        ("integration", "TaskIntegrationDetails"),
        ("invitation", "Invitation"),
    ] {
        assert_eq!(schema["properties"][field]["nullable"], true);
        assert_eq!(
            schema["properties"][field]["oneOf"],
            json!([{"$ref": format!("#/components/schemas/{component}")}])
        );
    }
    for (kind, field, value) in [
        ("EXPLORE_EXAMPLE_PROJECT", "segment", Value::Null),
        (
            "REVIEW_SEGMENTS",
            "segment",
            json!({"source_language_code":"en","target_language_code":null}),
        ),
        (
            "TRANSLATE_SEGMENTS",
            "segment",
            json!({"source_language_code":"en","target_language_code":"de"}),
        ),
        (
            "IMPORT_ISSUES",
            "import",
            json!({"import_id":"rin_0197c8f2-5a6b-7cde-8f12-3456789abcde","type":"UPLOAD","status":"FAILED","status_details":{"code":"UNKNOWN_ERROR","detail":"The operation failed without a more specific recorded cause."}}),
        ),
        (
            "INTEGRATION_ERROR",
            "integration",
            json!({"code":"REPOSITORY_URL_MISMATCH"}),
        ),
        (
            "INVITATION_PENDING",
            "invitation",
            json!({"object_type":"INVITATION","id":"svi_0197c8f2-5a6b-7cde-8f12-3456789abcde","workspace_role_id":"rol_0197c8f2-5a6b-7cde-8f12-3456789abcde","inviter_actor_id":"act_0197c8f2-5a6b-7cde-8f12-3456789abcde","invitee_user_id":null,"email":"invitee@example.test","invitee_name":null,"message":null,"is_stack_assignment_configured":false,"status":"PENDING","expires_at":"2027-01-01T00:00:00.000Z","accepted_at":null,"declined_at":null,"created_at":"2026-10-01T00:00:00.000Z","updated_at":"2026-10-01T00:00:00.000Z"}),
        ),
    ] {
        let mut expected = base.clone();
        expected["type"] = json!(kind);
        expected[field] = value;
        let stub = Stub::new(vec![response(
            "200 OK",
            expected.clone(),
            "ETag: \"task-details\"\r\n",
        )]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "getTaskDetails",
                "--path",
                &format!("taskId={id}"),
                "--include-headers",
            ],
            None,
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), expected);
        assert!(String::from_utf8_lossy(&out.stderr).contains("task-details"));
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("GET /v2/tasks/{id}/details ")));
        assert!(requests[0].split_once("\r\n\r\n").unwrap().1.is_empty());
    }
}

#[test]
fn task_details_preserve_current_access_and_strict_input_failures_without_retry() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("400 Bad Request", "unknown_parameter"),
    ] {
        let expected =
            json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"task-details"});
        let stub = Stub::new(vec![response(status, expected.clone(), "")]);
        let mut args = vec![
            "--json",
            "api",
            "call",
            "getTaskDetails",
            "--path",
            "taskId=task_0197c8f2-5a6b-7cde-8f12-3456789abcde",
        ];
        if code == "unknown_parameter" {
            args.extend(["--query", "unsupported=true"]);
        }
        let out = command(&stub.origin, &args, None);
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            expected
        );
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /v2/tasks/task_0197c8f2-5a6b-7cde-8f12-3456789abcde/details"));
        if code == "unknown_parameter" {
            assert!(requests[0].contains("unsupported=true"));
        }
    }
}

#[test]
fn highlight_attachment_preserves_exact_decimals_flag_replay_key_and_response() {
    let image = "ci_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let segment = "seg_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let contract: Value = serde_json::from_str(include_str!("../contract/v2.json")).unwrap();
    assert_eq!(
        contract["components"]["schemas"]["AttachContextImageHighlights"]["required"],
        json!(["highlight_boxes", "is_highlighted"])
    );
    assert_eq!(
        contract["components"]["schemas"]["SetContextImageHighlightBox"]["required"],
        json!(["height", "left", "top", "width"])
    );
    for field in ["left", "top", "width", "height"] {
        let property = &contract["components"]["schemas"]["SetContextImageHighlightBox"]["properties"][field];
        assert_eq!(property["multipleOf"].as_f64(), Some(1e-20));
        assert!(
            property["description"]
                .as_str()
                .unwrap()
                .contains("21 significant digits")
        );
        assert!(
            property["description"]
                .as_str()
                .unwrap()
                .contains("scale -20 through 20")
        );
    }
    let expected = json!({"object_type":"CONTEXT_IMAGE","id":image,"project_id":"pr_0197c8f2-5a6b-7cde-8f12-3456789abcde","language_code":"de","name":"Checkout","description":null,"url":"https://cdn.example.test/ci.png","width":800,"height":600,"upload_status":"DONE","content_hash":"abc","capture_key":null,"app_version":null,"captured_at":null,"attachments":[],"created_at":"2026-10-01T00:00:00.123Z","updated_at":"2026-10-01T00:00:00.456Z"});
    let required = contract["components"]["schemas"]["ContextImage"]["required"]
        .as_array()
        .unwrap();
    assert_eq!(expected.as_object().unwrap().len(), required.len());
    for field in required {
        assert!(expected.get(field.as_str().unwrap()).is_some());
    }
    for (highlights, replay_key) in [
        (
            r#"{"highlight_boxes":[{"left":0.99999999999999999999,"top":0,"width":0.00000000000000000001,"height":1}],"is_highlighted":true}"#,
            None,
        ),
        (
            r#"{"highlight_boxes":[{"left":0.75,"top":0.5,"width":0.25,"height":0.25},{"left":0.125,"top":0.25,"width":0.5,"height":0.1}],"is_highlighted":true}"#,
            Some("ordered"),
        ),
        (r#"{"highlight_boxes":[],"is_highlighted":true}"#, Some("cleared")),
        (
            r#"{"highlight_boxes":[],"is_highlighted":false}"#,
            Some("unhighlighted"),
        ),
    ] {
        let body = format!(r#"{{"segments":[{{"segment_id":"{segment}","highlights":{highlights}}}]}}"#);
        let input: Value = serde_json::from_str(&body).unwrap();
        let stub = Stub::new(vec![response(
            "200 OK",
            expected.clone(),
            "ETag: \"attached\"\r\n",
        )]);
        let path = format!("contextImageId={image}");
        let mut args = vec![
            "--json",
            "api",
            "call",
            "attachContextImage",
            "--path",
            &path,
            "--include-headers",
            "--file",
            "-",
        ];
        if let Some(value) = replay_key {
            args.extend(["--idempotency-key", value]);
        }
        let out = command(&stub.origin, &args, Some(&body));
        assert_eq!(success(&out), expected);
        assert!(String::from_utf8_lossy(&out.stderr).contains("etag: \"attached\""));
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("POST /v2/context-images/{image}/attach ")));
        let (headers, payload) = requests[0].split_once("\r\n\r\n").unwrap();
        let headers = headers.to_lowercase();
        match replay_key {
            Some(value) => assert!(headers.contains(&format!("idempotency-key: {value}"))),
            None => assert!(!headers.contains("idempotency-key:")),
        }
        assert!(!headers.contains("if-match:"));
        assert_eq!(serde_json::from_str::<Value>(payload).unwrap(), input);
    }
}

#[test]
fn highlight_attachment_preserves_problems_and_never_retries_the_write() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("409 Conflict", "idempotency_outcome_unknown"),
        ("422 Unprocessable Entity", "validation_failed"),
    ] {
        let problem = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"highlights","errors":[{"pointer":"#/segments/0/highlights/highlight_boxes/0/width","code":"not_supported","detail":"Left plus width must not exceed one."}]});
        let stub = Stub::new(vec![response(status, problem.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "attachContextImage",
                "--path",
                "contextImageId=ci_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--idempotency-key",
                "highlight-once",
                "--file",
                "-",
            ],
            Some(
                r#"{"segments":[{"segment_id":"seg_0197c8f2-5a6b-7cde-8f12-3456789abcde","highlights":{"highlight_boxes":[{"left":0.5,"top":0,"width":0.6,"height":1}],"is_highlighted":true}}]}"#,
            ),
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            problem
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn stack_permission_replacement_preserves_all_public_levels_and_conditions() {
    let contract: Value = serde_json::from_str(include_str!("../contract/v2.json")).unwrap();
    assert_eq!(
        contract["components"]["schemas"]["SetApiKeyStackPermissions"]["required"],
        json!([
            "activities",
            "context_images",
            "general",
            "languages",
            "resources",
            "segment_keys",
            "segment_texts",
            "segments",
            "settings"
        ])
    );
    let operation = &contract["paths"]["/v2/api-keys/{apiKeyId}/stack-permissions/{stackId}"]["put"];
    assert!(operation["responses"]["200"]["headers"].get("ETag").is_none());
    let api = "api_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let stack = "stack_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let body = json!({"general":"VIEW","settings":"MANAGE","activities":"NO_ACCESS","resources":"VIEW","context_images":"MANAGE","languages":"NO_ACCESS","segment_keys":"VIEW","segments":"MANAGE","segment_texts":"NO_ACCESS"});
    let permissions: Vec<Value> = body
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, level)| json!({"scope":"STACK","target":name.to_uppercase(),"level":level}))
        .collect();
    let expected = json!({"object_type":"API_KEY_STACK_PERMISSIONS","api_key_id":api,"stack_id":stack,"permissions":permissions,"created_at":"2026-10-01T00:00:00.123Z","updated_at":"2026-10-01T01:00:00.456Z"});
    let required = contract["components"]["schemas"]["ApiKeyStackPermissions"]["required"]
        .as_array()
        .unwrap();
    assert_eq!(expected.as_object().unwrap().len(), required.len());
    for field in required {
        assert!(expected.get(field.as_str().unwrap()).is_some());
    }
    for condition in [None, Some("*"), Some("\"override\"")] {
        let stub = Stub::new(vec![response("200 OK", expected.clone(), "")]);
        let api_path = format!("apiKeyId={api}");
        let stack_path = format!("stackId={stack}");
        let mut args = vec![
            "--json",
            "api",
            "call",
            "setWorkspaceApiKeyStackPermissions",
            "--path",
            &api_path,
            "--path",
            &stack_path,
            "--file",
            "-",
            "--include-headers",
        ];
        if let Some(value) = condition {
            args.extend(["--if-match", value]);
        }
        let out = command(&stub.origin, &args, Some(&body.to_string()));
        assert_eq!(success(&out), expected);
        assert!(
            !String::from_utf8_lossy(&out.stderr)
                .to_lowercase()
                .contains("etag:")
        );
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("PUT /v2/api-keys/{api}/stack-permissions/{stack} ")));
        let (headers, payload) = requests[0].split_once("\r\n\r\n").unwrap();
        assert_eq!(serde_json::from_str::<Value>(payload).unwrap(), body);
        match condition {
            Some(value) => assert!(headers.to_lowercase().contains(&format!("if-match: {value}"))),
            None => assert!(!headers.to_lowercase().contains("if-match:")),
        }
    }
}

#[test]
fn stack_permission_replacement_retains_errors_without_retrying() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("412 Precondition Failed", "precondition_failed"),
        ("422 Unprocessable Content", "validation_failed"),
    ] {
        let mut error = json!({
            "type": "about:blank",
            "title": &status[4..],
            "status": status[..3].parse::<u16>().unwrap(),
            "detail": "The permission replacement could not be completed.",
            "instance": "/v2/api-keys/api_0197c8f2-5a6b-7cde-8f12-3456789abcde/stack-permissions/stack_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            "code": code,
            "request_id": "override-permissions",
            "errors": []
        });
        if code == "validation_failed" {
            error["errors"] = json!([{"pointer":"#/segment_texts","parameter":null,"code":"required","detail":"A level is required."}]);
        }
        let stub = Stub::new(vec![response(status, error.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "setWorkspaceApiKeyStackPermissions",
                "--path",
                "apiKeyId=api_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--path",
                "stackId=stack_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--file",
                "-",
            ],
            Some("{}"),
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let actual: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(actual["problem"], error);
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn default_stack_permission_replacement_preserves_all_public_levels_and_conditions() {
    let contract: Value = serde_json::from_str(include_str!("../contract/v2.json")).unwrap();
    assert_eq!(
        contract["components"]["schemas"]["SetApiKeyDefaultStackPermissions"]["required"],
        json!([
            "activities",
            "context_images",
            "general",
            "languages",
            "resources",
            "segment_keys",
            "segment_texts",
            "segments",
            "settings"
        ])
    );
    let operation = &contract["paths"]["/v2/api-keys/{apiKeyId}/default-stack-permissions"]["put"];
    assert!(operation["responses"]["200"]["headers"].get("ETag").is_none());
    let id = "api_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let body = json!({"general":"VIEW","settings":"MANAGE","activities":"NO_ACCESS","resources":"VIEW","context_images":"MANAGE","languages":"NO_ACCESS","segment_keys":"VIEW","segments":"MANAGE","segment_texts":"NO_ACCESS"});
    let permissions: Vec<Value> = [
        "GENERAL",
        "SETTINGS",
        "ACTIVITIES",
        "RESOURCES",
        "CONTEXT_IMAGES",
        "LANGUAGES",
        "SEGMENT_KEYS",
        "SEGMENTS",
        "SEGMENT_TEXTS",
    ]
    .iter()
    .map(|target| json!({"scope":"STACK","target":target,"level":body[target.to_lowercase()]}))
    .collect();
    let workspace_permissions: Vec<Value> = [
        "BILLING_SUBSCRIPTION",
        "WORKSPACES",
        "MEMBERS",
        "PLUGINS_INTEGRATIONS",
        "LOGS",
        "API_ACCESS",
        "STACKS",
        "TRANSLATION_ORDERS",
        "WORK_REPORTS",
        "STYLE_GUIDES",
        "GLOSSARIES",
        "TRANSLATION_MEMORIES",
        "LABELS",
        "AI_TRANSLATION",
    ]
    .iter()
    .map(|target| json!({"scope":"WORKSPACE","target":target,"level":"NO_ACCESS"}))
    .collect();
    let expected = json!({"object_type":"API_KEY_PERMISSION_CONFIGURATION","id":id,"management_mode":"WORKSPACE_MANAGED","workspace_permissions":workspace_permissions,"default_stack_permissions":permissions,"created_at":"2026-10-01T00:00:00.123Z","updated_at":"2026-10-01T01:00:00.456Z"});
    let required = contract["components"]["schemas"]["ApiKeyPermissionConfiguration"]["required"]
        .as_array()
        .unwrap();
    assert_eq!(expected.as_object().unwrap().len(), required.len());
    for field in required {
        assert!(expected.get(field.as_str().unwrap()).is_some());
    }
    for condition in [None, Some("*"), Some("\"override\"")] {
        let stub = Stub::new(vec![response("200 OK", expected.clone(), "")]);
        let path = format!("apiKeyId={id}");
        let mut args = vec![
            "--json",
            "api",
            "call",
            "setWorkspaceApiKeyDefaultStackPermissions",
            "--path",
            &path,
            "--file",
            "-",
            "--include-headers",
        ];
        if let Some(value) = condition {
            args.extend(["--if-match", value]);
        }
        let out = command(&stub.origin, &args, Some(&body.to_string()));
        assert_eq!(success(&out), expected);
        assert!(
            !String::from_utf8_lossy(&out.stderr)
                .to_lowercase()
                .contains("etag:")
        );
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("PUT /v2/api-keys/{id}/default-stack-permissions ")));
        let (headers, payload) = requests[0].split_once("\r\n\r\n").unwrap();
        assert_eq!(serde_json::from_str::<Value>(payload).unwrap(), body);
        match condition {
            Some(value) => assert!(headers.to_lowercase().contains(&format!("if-match: {value}"))),
            None => assert!(!headers.to_lowercase().contains("if-match:")),
        }
    }
}

#[test]
fn default_stack_permission_replacement_retains_errors_without_retrying() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("412 Precondition Failed", "precondition_failed"),
        ("422 Unprocessable Content", "validation_failed"),
    ] {
        let mut error = json!({
            "type": "about:blank",
            "title": &status[4..],
            "status": status[..3].parse::<u16>().unwrap(),
            "detail": "The permission replacement could not be completed.",
            "instance": "/v2/api-keys/api_0197c8f2-5a6b-7cde-8f12-3456789abcde/default-stack-permissions",
            "code": code,
            "request_id": "override-permissions",
            "errors": []
        });
        if code == "validation_failed" {
            error["errors"] = json!([{"pointer":"#/segment_texts","parameter":null,"code":"required","detail":"A level is required."}]);
        }
        let stub = Stub::new(vec![response(status, error.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "setWorkspaceApiKeyDefaultStackPermissions",
                "--path",
                "apiKeyId=api_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--file",
                "-",
            ],
            Some("{}"),
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let actual: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(actual["problem"], error);
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn workspace_permission_replacement_preserves_all_public_levels_and_conditions() {
    let contract: Value = serde_json::from_str(include_str!("../contract/v2.json")).unwrap();
    assert_eq!(
        contract["components"]["schemas"]["SetApiKeyWorkspacePermissions"]["required"],
        json!([
            "ai_translation",
            "api_access",
            "billing_subscription",
            "glossaries",
            "labels",
            "logs",
            "members",
            "plugins_integrations",
            "stacks",
            "style_guides",
            "translation_memories",
            "translation_orders",
            "work_reports",
            "workspaces"
        ])
    );
    let operation = &contract["paths"]["/v2/api-keys/{apiKeyId}/workspace-permissions"]["put"];
    assert!(operation["responses"]["200"]["headers"].get("ETag").is_none());
    let id = "api_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let body = json!({"billing_subscription":"NO_ACCESS","workspaces":"NO_ACCESS","members":"NO_ACCESS","plugins_integrations":"VIEW","logs":"MANAGE","api_access":"MANAGE","stacks":"VIEW","translation_orders":"MANAGE","work_reports":"NO_ACCESS","style_guides":"VIEW","glossaries":"MANAGE","translation_memories":"NO_ACCESS","labels":"VIEW","ai_translation":"MANAGE"});
    let mut workspace_permissions: Vec<Value> = [
        "BILLING_SUBSCRIPTION",
        "WORKSPACES",
        "MEMBERS",
        "PLUGINS_INTEGRATIONS",
        "LOGS",
        "API_ACCESS",
        "STACKS",
        "TRANSLATION_ORDERS",
        "WORK_REPORTS",
        "STYLE_GUIDES",
        "GLOSSARIES",
        "TRANSLATION_MEMORIES",
        "LABELS",
        "AI_TRANSLATION",
    ]
    .iter()
    .map(|target| json!({"scope":"WORKSPACE","target":target,"level":body[target.to_lowercase()]}))
    .collect();
    workspace_permissions.push(json!({"scope":"WORKSPACE","target":"QUICK_TRANSLATIONS","level":"WRITE"}));
    let permissions: Vec<Value> = [
        "GENERAL",
        "SETTINGS",
        "ACTIVITIES",
        "RESOURCES",
        "CONTEXT_IMAGES",
        "LANGUAGES",
        "SEGMENT_KEYS",
        "SEGMENTS",
        "SEGMENT_TEXTS",
    ]
    .iter()
    .map(|target| json!({"scope":"STACK","target":target,"level":"VIEW"}))
    .collect();
    let expected = json!({"object_type":"API_KEY_PERMISSION_CONFIGURATION","id":id,"management_mode":"WORKSPACE_MANAGED","workspace_permissions":workspace_permissions,"default_stack_permissions":permissions,"created_at":"2026-10-01T00:00:00.123Z","updated_at":"2026-10-01T01:00:00.456Z"});
    let required = contract["components"]["schemas"]["ApiKeyPermissionConfiguration"]["required"]
        .as_array()
        .unwrap();
    assert_eq!(expected.as_object().unwrap().len(), required.len());
    for field in required {
        assert!(expected.get(field.as_str().unwrap()).is_some());
    }
    for condition in [None, Some("*"), Some("\"override\"")] {
        let stub = Stub::new(vec![response("200 OK", expected.clone(), "")]);
        let path = format!("apiKeyId={id}");
        let mut args = vec![
            "--json",
            "api",
            "call",
            "setWorkspaceApiKeyWorkspacePermissions",
            "--path",
            &path,
            "--file",
            "-",
            "--include-headers",
        ];
        if let Some(value) = condition {
            args.extend(["--if-match", value]);
        }
        let out = command(&stub.origin, &args, Some(&body.to_string()));
        assert_eq!(success(&out), expected);
        assert!(
            !String::from_utf8_lossy(&out.stderr)
                .to_lowercase()
                .contains("etag:")
        );
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("PUT /v2/api-keys/{id}/workspace-permissions ")));
        let (headers, payload) = requests[0].split_once("\r\n\r\n").unwrap();
        assert_eq!(serde_json::from_str::<Value>(payload).unwrap(), body);
        match condition {
            Some(value) => assert!(headers.to_lowercase().contains(&format!("if-match: {value}"))),
            None => assert!(!headers.to_lowercase().contains("if-match:")),
        }
    }
}

#[test]
fn workspace_permission_replacement_retains_errors_without_retrying() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("412 Precondition Failed", "precondition_failed"),
        ("422 Unprocessable Content", "validation_failed"),
    ] {
        let mut error = json!({
            "type": "about:blank",
            "title": &status[4..],
            "status": status[..3].parse::<u16>().unwrap(),
            "detail": "The permission replacement could not be completed.",
            "instance": "/v2/api-keys/api_0197c8f2-5a6b-7cde-8f12-3456789abcde/workspace-permissions",
            "code": code,
            "request_id": "override-permissions",
            "errors": []
        });
        if code == "validation_failed" {
            error["errors"] = json!([{"pointer":"#/api_access","parameter":null,"code":"required","detail":"A level is required."}]);
        }
        let stub = Stub::new(vec![response(status, error.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "setWorkspaceApiKeyWorkspacePermissions",
                "--path",
                "apiKeyId=api_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--file",
                "-",
            ],
            Some("{}"),
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let actual: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(actual["problem"], error);
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn stack_permission_creation_forwards_complete_body_and_location() {
    let contract: Value = serde_json::from_str(include_str!("../contract/v2.json")).unwrap();
    let operation = &contract["paths"]["/v2/api-keys/{apiKeyId}/stack-permissions/{stackId}"]["put"];
    assert!(operation["responses"]["201"]["headers"].get("Location").is_some());
    // Creation is an upsert at a known key, so there is no idempotency key to forward.
    assert!(
        operation["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .all(|parameter| parameter["name"] != "Idempotency-Key")
    );
    let api = "api_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let stack = "stack_0197c8f2-5a6b-7cde-8f12-3456789abcdf";
    let body = json!({"general":"VIEW","settings":"NO_ACCESS","activities":"MANAGE","resources":"VIEW","context_images":"MANAGE","languages":"NO_ACCESS","segment_keys":"VIEW","segments":"MANAGE","segment_texts":"NO_ACCESS"});
    let permissions: Vec<_> = [
        "GENERAL",
        "SETTINGS",
        "ACTIVITIES",
        "RESOURCES",
        "CONTEXT_IMAGES",
        "LANGUAGES",
        "SEGMENT_KEYS",
        "SEGMENTS",
        "SEGMENT_TEXTS",
    ]
    .iter()
    .map(|target| json!({"scope":"STACK","target":target,"level":body[target.to_lowercase()]}))
    .collect();
    let expected = json!({"object_type":"API_KEY_STACK_PERMISSIONS","api_key_id":api,"stack_id":stack,"permissions":permissions,"created_at":"2026-10-01T00:00:00.123Z","updated_at":"2026-10-01T00:00:00.123Z"});
    let fields = contract["components"]["schemas"]["ApiKeyStackPermissions"]["required"]
        .as_array()
        .unwrap();
    assert_eq!(expected.as_object().unwrap().len(), fields.len());
    for field in fields {
        assert!(expected.get(field.as_str().unwrap()).is_some());
    }
    let location = format!("/v2/api-keys/{api}/stack-permissions/{stack}");
    let stub = Stub::new(vec![response(
        "201 Created",
        expected.clone(),
        &format!("Location: {location}\r\n"),
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "setWorkspaceApiKeyStackPermissions",
            "--path",
            &format!("apiKeyId={api}"),
            "--path",
            &format!("stackId={stack}"),
            "--file",
            "-",
        ],
        Some(&body.to_string()),
    );
    assert_eq!(success(&out), expected);
    assert!(String::from_utf8_lossy(&out.stderr).contains(&format!("location: {location}")));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!("PUT {location} ")));
    let (headers, payload) = requests[0].split_once("\r\n\r\n").unwrap();
    assert_eq!(serde_json::from_str::<Value>(payload).unwrap(), body);
    assert!(!headers.to_lowercase().contains("idempotency-key:"));
    assert!(!headers.to_lowercase().contains("if-match:"));
}

#[test]
fn placeholder_replacement_preserves_all_profiles_order_empty_and_condition() {
    let contract: Value = serde_json::from_str(include_str!("../contract/v2.json")).unwrap();
    assert_eq!(
        contract["components"]["schemas"]["SetProjectPlaceholderSettings"]["required"],
        json!(["definitions"])
    );
    let all = contract["components"]["schemas"]["PlaceholderDefinition"]["enum"].clone();
    assert_eq!(all.as_array().unwrap().len(), 28);
    let id = "pr_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    for definitions in [json!(["IOS_PRINTF", "DEFAULT_ICU", "IOS_PRINTF"]), json!([]), all] {
        let body = json!({"definitions":definitions});
        let expected = json!({"object_type":"PROJECT_PLACEHOLDER_SETTINGS","id":id,"definitions":definitions,"default_definitions":["DEFAULT_ICU"],"is_default":false,"created_at":"2026-10-01T00:00:00.123Z","updated_at":"2026-10-01T01:00:00.456Z"});
        let fields = contract["components"]["schemas"]["ProjectPlaceholderSettings"]["required"]
            .as_array()
            .unwrap();
        assert_eq!(expected.as_object().unwrap().len(), fields.len());
        for field in fields {
            assert!(expected.get(field.as_str().unwrap()).is_some());
        }
        for condition in [None, Some("*"), Some("\"profiles\"")] {
            let stub = Stub::new(vec![response("200 OK", expected.clone(), "")]);
            let path = format!("projectId={id}");
            let mut args = vec![
                "--json",
                "--include-headers",
                "api",
                "call",
                "setProjectPlaceholderSettings",
                "--path",
                &path,
                "--file",
                "-",
            ];
            if let Some(value) = condition {
                args.extend(["--if-match", value]);
            }
            let out = command(&stub.origin, &args, Some(&body.to_string()));
            assert_eq!(success(&out), expected);
            assert!(
                !String::from_utf8_lossy(&out.stderr)
                    .to_lowercase()
                    .contains("etag:")
            );
            let requests = stub.finish();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].starts_with(&format!("PUT /v2/projects/{id}/placeholder-settings ")));
            let (headers, payload) = requests[0].split_once("\r\n\r\n").unwrap();
            assert_eq!(serde_json::from_str::<Value>(payload).unwrap(), body);
            match condition {
                Some(value) => assert!(headers.to_lowercase().contains(&format!("if-match: {value}"))),
                None => assert!(!headers.to_lowercase().contains("if-match:")),
            }
        }
    }
}

#[test]
fn placeholder_replacement_retains_errors_without_retrying() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("412 Precondition Failed", "precondition_failed"),
        ("422 Unprocessable Content", "validation_failed"),
    ] {
        let errors = if code == "validation_failed" {
            json!([{"pointer":"#/definitions/0","parameter":null,"code":"required","detail":"A supported profile is required."}])
        } else {
            json!([])
        };
        let error = json!({"type":"about:blank","title":&status[4..],"status":status[..3].parse::<u16>().unwrap(),"detail":"The profile replacement could not be completed.","instance":"/v2/projects/pr_0197c8f2-5a6b-7cde-8f12-3456789abcde/placeholder-settings","code":code,"request_id":"placeholder-replace","errors":errors});
        let stub = Stub::new(vec![response(status, error.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "setProjectPlaceholderSettings",
                "--path",
                "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--file",
                "-",
            ],
            Some("{}"),
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let actual: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(actual["problem"], error);
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn key_setting_replacement_preserves_strategy_and_condition() {
    let contract: Value = serde_json::from_str(include_str!("../contract/v2.json")).unwrap();
    assert_eq!(
        contract["components"]["schemas"]["SetProjectKeySettings"]["required"],
        json!(["normalization_strategy"])
    );
    let id = "pr_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    for definitions in [json!("ALL"), json!("NONE"), json!("FORMAT_SPECIFIC")] {
        let body = json!({"normalization_strategy":definitions});
        let expected = json!({"object_type":"PROJECT_KEY_SETTINGS","id":id,"normalization_strategy":definitions,"created_at":"2026-10-01T00:00:00.123Z","updated_at":"2026-10-01T01:00:00.456Z"});
        let fields = contract["components"]["schemas"]["ProjectKeySettings"]["required"]
            .as_array()
            .unwrap();
        assert_eq!(expected.as_object().unwrap().len(), fields.len());
        for field in fields {
            assert!(expected.get(field.as_str().unwrap()).is_some());
        }
        for condition in [None, Some("*"), Some("\"strategy\"")] {
            let stub = Stub::new(vec![response("200 OK", expected.clone(), "")]);
            let path = format!("projectId={id}");
            let mut args = vec![
                "--json",
                "--include-headers",
                "api",
                "call",
                "setProjectKeySettings",
                "--path",
                &path,
                "--file",
                "-",
            ];
            if let Some(value) = condition {
                args.extend(["--if-match", value]);
            }
            let out = command(&stub.origin, &args, Some(&body.to_string()));
            assert_eq!(success(&out), expected);
            assert!(
                !String::from_utf8_lossy(&out.stderr)
                    .to_lowercase()
                    .contains("etag:")
            );
            let requests = stub.finish();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].starts_with(&format!("PUT /v2/projects/{id}/key-settings ")));
            let (headers, payload) = requests[0].split_once("\r\n\r\n").unwrap();
            assert_eq!(serde_json::from_str::<Value>(payload).unwrap(), body);
            match condition {
                Some(value) => assert!(headers.to_lowercase().contains(&format!("if-match: {value}"))),
                None => assert!(!headers.to_lowercase().contains("if-match:")),
            }
        }
    }
}

#[test]
fn key_setting_replacement_retains_errors_without_retrying() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("412 Precondition Failed", "precondition_failed"),
        ("422 Unprocessable Content", "validation_failed"),
    ] {
        let errors = if code == "validation_failed" {
            json!([{"pointer":"#/normalization_strategy","parameter":null,"code":"required","detail":"A supported strategy is required."}])
        } else {
            json!([])
        };
        let error = json!({"type":"about:blank","title":&status[4..],"status":status[..3].parse::<u16>().unwrap(),"detail":"The key-strategy replacement could not be completed.","instance":"/v2/projects/pr_0197c8f2-5a6b-7cde-8f12-3456789abcde/key-settings","code":code,"request_id":"key-replace","errors":errors});
        let stub = Stub::new(vec![response(status, error.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "setProjectKeySettings",
                "--path",
                "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--file",
                "-",
            ],
            Some("{}"),
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let actual: Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(actual["problem"], error);
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn task_contributors_preserve_public_relations_filters_and_cursor() {
    let contract: Value = serde_json::from_str(include_str!("../contract/v2.json")).unwrap();
    let schema = &contract["components"]["schemas"]["TaskContributor"];
    assert_eq!(
        schema["required"],
        json!([
            "is_source",
            "language_code",
            "object_type",
            "stack_id",
            "stack_language_id"
        ])
    );
    assert!(schema["properties"].get("id").is_none());
    let filter = json!({"where":{"task_contributor[is_source]":{"_true":true}},"sort":["+task_contributor[language_code]"]}).to_string();
    let item = json!({"object_type":"TASK_CONTRIBUTOR","stack_id":"stack_0197c8f2-5a6b-7cde-8f12-3456789abcde","stack_language_id":"stackl_0197c8f2-5a6b-7cde-8f12-3456789abcde","language_code":"en","is_source":true});
    let expected = page(json!([item]), Some("next-cursor+/="));
    let stub = Stub::new(vec![response("200 OK", expected.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listTaskContributors",
            "--path",
            "taskId=task_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            "--query",
            &format!("filter={filter}"),
            "--query",
            "limit=1",
            "--query",
            "cursor=previous-cursor+/=",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), expected);
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    let target = reqwest::Url::parse(&format!(
        "http://localhost{}",
        requests[0].split_whitespace().nth(1).unwrap()
    ))
    .unwrap();
    assert_eq!(
        target.path(),
        "/v2/tasks/task_0197c8f2-5a6b-7cde-8f12-3456789abcde/contributors"
    );
    let query: std::collections::BTreeMap<_, _> = target.query_pairs().into_owned().collect();
    assert_eq!(query.get("filter"), Some(&filter));
    assert_eq!(query.get("limit").map(String::as_str), Some("1"));
    assert_eq!(
        query.get("cursor").map(String::as_str),
        Some("previous-cursor+/=")
    );
    assert!(requests[0].split_once("\r\n\r\n").unwrap().1.is_empty());
}

#[test]
fn task_contributors_preserve_current_access_and_paging_failures_without_retry() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("400 Bad Request", "invalid_cursor"),
        ("400 Bad Request", "invalid_filter"),
        ("400 Bad Request", "invalid_parameter"),
    ] {
        let expected = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"task-contributors"});
        let stub = Stub::new(vec![response(status, expected.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "listTaskContributors",
                "--path",
                "taskId=task_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            expected
        );
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].starts_with("GET /v2/tasks/task_0197c8f2-5a6b-7cde-8f12-3456789abcde/contributors")
        );
    }
}

#[test]
fn import_history_preserves_cursor_queries_and_stored_values() {
    for (operation, suffix, item) in [
        (
            "listImportResourceChanges",
            "resource",
            json!({"object_type":"IMPORT_RESOURCE_CHANGE","id":"rsl_0197c8f2-5a6b-7cde-8f12-3456789abcde","entry_id":"rie_0197c8f2-5a6b-7cde-8f12-3456789abcde","segment_id":null,"key_id":null,"type":"RENAMED","previous_path":"old/de.json","path":"de.json","previous_key":"old.key","key":"new.key","created_at":"2026-10-01T10:00:00.000Z","updated_at":"2026-10-01T10:00:00.000Z"}),
        ),
        (
            "listImportTextChanges",
            "text",
            json!({"object_type":"IMPORT_TEXT_CHANGE","id":"sll_0197c8f2-5a6b-7cde-8f12-3456789abcde","entry_id":"rie_0197c8f2-5a6b-7cde-8f12-3456789abcde","segment_id":null,"text_id":null,"type":"CONTENT_CHANGED","language_code":"de","previous_status":"TRANSLATED","status":"TRANSLATED","variants":[{"position":1,"device":"OTHER","plural_form":"OTHER","previous_content":"Before <tag>","content":"After & value"}],"created_at":"2026-10-01T10:00:00.000Z","updated_at":"2026-10-01T10:00:00.000Z"}),
        ),
    ] {
        let field = format!("import_{suffix}_change[entry_id]");
        let filter = json!({"where":{"_and":[{field:{"_eq":"rie_0197c8f2-5a6b-7cde-8f12-3456789abcde"}}]}})
            .to_string();
        let expected = page(json!([item]), Some("next-cursor+/="));
        let stub = Stub::new(vec![
            response("200 OK", expected.clone(), ""),
            response("200 OK", page(json!([]), None), ""),
        ]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                operation,
                "--path",
                "importId=rin_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--query",
                &format!("filter={filter}"),
                "--query",
                "limit=1",
            ],
            None,
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), expected);
        let next = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                operation,
                "--path",
                "importId=rin_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--query",
                &format!("filter={filter}"),
                "--query",
                "limit=1",
                "--query",
                "cursor=next-cursor+/=",
            ],
            None,
        );
        assert!(next.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&next.stdout).unwrap(),
            page(json!([]), None)
        );
        let requests = stub.finish();
        assert_eq!(requests.len(), 2);
        for (index, request) in requests.iter().enumerate() {
            assert!(request.starts_with("GET "));
            let target = reqwest::Url::parse(&format!(
                "http://localhost{}",
                request.split_whitespace().nth(1).unwrap()
            ))
            .unwrap();
            assert_eq!(
                target.path(),
                format!("/v2/imports/rin_0197c8f2-5a6b-7cde-8f12-3456789abcde/{suffix}-changes")
            );
            let query: std::collections::BTreeMap<_, _> = target.query_pairs().into_owned().collect();
            assert_eq!(query.get("filter"), Some(&filter));
            assert_eq!(query.get("limit").map(String::as_str), Some("1"));
            assert_eq!(
                query.get("cursor").map(String::as_str),
                if index == 0 { None } else { Some("next-cursor+/=") }
            );
            assert!(request.split_once("\r\n\r\n").unwrap().1.is_empty());
        }
    }
}

#[test]
fn import_history_preserves_access_and_paging_problems_without_retry() {
    for operation in ["listImportResourceChanges", "listImportTextChanges"] {
        for (status, code) in [
            ("401 Unauthorized", "missing_api_key"),
            ("403 Forbidden", "insufficient_permissions"),
            ("404 Not Found", "resource_not_found"),
            ("400 Bad Request", "invalid_cursor"),
            ("400 Bad Request", "invalid_filter"),
        ] {
            let expected = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"import-history"});
            let stub = Stub::new(vec![response(status, expected.clone(), "")]);
            let out = command(
                &stub.origin,
                &[
                    "--json",
                    "api",
                    "call",
                    operation,
                    "--path",
                    "importId=rin_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                ],
                None,
            );
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            assert_eq!(
                serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
                expected
            );
            assert_eq!(stub.finish().len(), 1);
        }
    }
}

#[test]
fn invitation_assignment_preserves_complete_availability_and_nullable_defaults() {
    let id = "svi_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    for (availability, assignment) in [
        ("NOT_CONFIGURED", Value::Null),
        ("UNAVAILABLE", Value::Null),
        (
            "AVAILABLE",
            json!({"stack_role_id":"stackr_0197c8f2-5a6b-7cde-8f12-3456789abcde","stack_ids":["stack_0197c8f2-5a6b-7cde-8f12-3456789abcde"],"language_permissions":null}),
        ),
        (
            "AVAILABLE",
            json!({"stack_role_id":"stackr_0197c8f2-5a6b-7cde-8f12-3456789abcde","stack_ids":["stack_0197c8f2-5a6b-7cde-8f12-3456789abcde"],"language_permissions":{}}),
        ),
        (
            "AVAILABLE",
            json!({"stack_role_id":"stackr_0197c8f2-5a6b-7cde-8f12-3456789abcde","stack_ids":["stack_0197c8f2-5a6b-7cde-8f12-3456789abcde"],"language_permissions":{"de":"RESTRICTED_ACCESS"}}),
        ),
    ] {
        let expected = json!({"object_type":"INVITATION_STACK_ASSIGNMENT","id":id,"availability":availability,"assignment":assignment});
        let stub = Stub::new(vec![response(
            "200 OK",
            expected.clone(),
            "ETag: \"assignment-current\"\r\n",
        )]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                "getInvitationStackAssignment",
                "--path",
                &format!("invitationId={id}"),
            ],
            None,
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), expected);
        assert!(String::from_utf8_lossy(&out.stderr).contains("assignment-current"));
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("GET /v2/invitations/{id}/stack-assignment ")));
        assert!(requests[0].split_once("\r\n\r\n").unwrap().1.is_empty());
    }
}

#[test]
fn invitation_assignment_preserves_scope_and_authentication_errors_without_retry() {
    for (status, code) in [
        ("401 Unauthorized", "disabled_api_key"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("400 Bad Request", "invalid_parameter"),
    ] {
        let expected = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"invitation-assignment"});
        let stub = Stub::new(vec![response(status, expected.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "getInvitationStackAssignment",
                "--path",
                "invitationId=svi_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            expected
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn file_settings_replacement_preserves_complete_values_and_validator() {
    let id = "pr_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    for (kind, operation, payload) in [
        (
            "import",
            "setProjectImportSettings",
            json!({"is_source_creation_enabled":false,"source_text_update_mode":"NEVER","is_source_metadata_update_enabled":false,"is_source_deactivation_enabled":false,"is_target_creation_enabled":true,"target_text_update_mode":"ALWAYS","is_target_metadata_update_enabled":true,"is_target_deactivation_enabled":true}),
        ),
        (
            "export",
            "setProjectExportSettings",
            json!({"source_statuses":["APPROVED","NEW","APPROVED"],"target_statuses":[],"source_empty_content_handling":"SKIP","target_empty_content_handling":"KEEP","is_not_translatable_skipped":false,"html_encoding":"RAW"}),
        ),
    ] {
        let expected = json!({"object_type":format!("PROJECT_{}_SETTINGS",kind.to_uppercase()),"id":id,"configuration":payload,"created_at":"2026-10-01T00:00:00.000Z","updated_at":"2026-10-01T10:00:00.000Z"});
        let stub = Stub::new(vec![response("200 OK", expected.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "--include-headers",
                "api",
                "call",
                operation,
                "--path",
                &format!("projectId={id}"),
                "--if-match",
                "\"current-defaults\"",
                "--file",
                "-",
            ],
            Some(&payload.to_string()),
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), expected);
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("PUT /v2/projects/{id}/{kind}-settings ")));
        assert!(
            requests[0]
                .to_lowercase()
                .contains("if-match: \"current-defaults\"")
        );
        assert_eq!(
            serde_json::from_str::<Value>(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap(),
            payload
        );
    }
}

#[test]
fn file_settings_replacement_preserves_current_scope_stale_and_field_problems_without_retry() {
    for operation in ["setProjectImportSettings", "setProjectExportSettings"] {
        for (status, code) in [
            ("401 Unauthorized", "disabled_api_key"),
            ("403 Forbidden", "insufficient_permissions"),
            ("404 Not Found", "resource_not_found"),
            ("412 Precondition Failed", "precondition_failed"),
            ("422 Unprocessable Entity", "validation_failed"),
        ] {
            let expected = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"file-defaults","errors":[{"pointer":"#/source_statuses/0","code":"enum","detail":"Unknown status"}]});
            let stub = Stub::new(vec![response(status, expected.clone(), "")]);
            let out = command(
                &stub.origin,
                &[
                    "--json",
                    "api",
                    "call",
                    operation,
                    "--path",
                    "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                    "--file",
                    "-",
                ],
                Some("{}"),
            );
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            assert_eq!(
                serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
                expected
            );
            assert_eq!(stub.finish().len(), 1);
        }
    }
}

#[test]
fn workspace_key_identity_filters_preserve_public_values_and_cursor_queries() {
    for (field, prefix) in [("stack_id", "stack"), ("member_id", "wsu"), ("user_id", "usr")] {
        let identity = format!("{prefix}_019b76da-a584-7000-8000-000000000001");
        let filter =
            json!({"where":{"_and":[{format!("workspace_api_key[{field}]"):{"_eq":identity}}]}}).to_string();
        let first = json!({"object_type":"LIST","data":[{"object_type":"WORKSPACE_API_KEY","id":"api_019b76da-a584-7000-8000-000000000001",field:identity}],"has_more":true,"next_cursor":"opaque+/="});
        let last = json!({"object_type":"LIST","data":[],"has_more":false,"next_cursor":null});
        let stub = Stub::new(vec![
            response("200 OK", first.clone(), ""),
            response("200 OK", last.clone(), ""),
        ]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "listWorkspaceApiKeys",
                "--query",
                &format!("filter={filter}"),
                "--query",
                "limit=1",
            ],
            None,
        );
        assert_eq!(success(&out), first);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "listWorkspaceApiKeys",
                "--query",
                &format!("filter={filter}"),
                "--query",
                "limit=1",
                "--query",
                "cursor=opaque+/=",
            ],
            None,
        );
        assert_eq!(success(&out), last);
        let requests = stub.finish();
        assert_eq!(requests.len(), 2);
        for (index, request) in requests.iter().enumerate() {
            let url = reqwest::Url::parse(&format!(
                "http://localhost{}",
                request.split_whitespace().nth(1).unwrap()
            ))
            .unwrap();
            assert_eq!(url.path(), "/v2/api-keys");
            assert!(
                url.query_pairs()
                    .any(|(key, value)| key == "filter" && value == filter)
            );
            assert!(
                url.query_pairs()
                    .any(|(key, value)| key == "limit" && value == "1")
            );
            assert_eq!(
                url.query_pairs()
                    .any(|(key, value)| key == "cursor" && value == "opaque+/="),
                index == 1
            );
        }
    }
}

#[test]
fn workspace_key_identity_filter_failures_preserve_problem_details_without_retry() {
    for (status, code) in [
        ("400 Bad Request", "invalid_filter"),
        ("400 Bad Request", "invalid_cursor"),
        ("403 Forbidden", "insufficient_permissions"),
    ] {
        let body = json!({"type":"about:blank","title":"Cannot list keys","status":if code == "insufficient_permissions" {403} else {400},"detail":"Current query is rejected","code":code,"request_id":"identity-filter"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "listWorkspaceApiKeys",
                "--query",
                "filter={\"where\":{\"_and\":[{\"workspace_api_key[user_id]\":{\"_eq\":\"usr_019b76da-a584-7000-8000-000000000001\"}}]}}",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn git_integration_deletion_forwards_condition_and_accepts_no_content() {
    for condition in [None, Some("\"key-version\""), Some("*")] {
        let stub = Stub::new(vec![
            "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".into(),
        ]);
        let mut args = vec![
            "--json",
            "api",
            "call",
            "deleteProjectGitIntegration",
            "--path",
            "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
        ];
        if let Some(value) = condition {
            args.extend(["--if-match", value]);
        }
        let out = command(&stub.origin, &args, None);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(out.stdout.is_empty());
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0]
                .starts_with("DELETE /v2/projects/pr_0197c8f2-5a6b-7cde-8f12-3456789abcde/integrations/git ")
        );
        let (headers, body) = requests[0].split_once("\r\n\r\n").unwrap();
        assert!(body.is_empty());
        match condition {
            Some(value) => assert!(headers.to_lowercase().contains(&format!("if-match: {value}"))),
            None => assert!(!headers.to_lowercase().contains("if-match:")),
        }
    }
}

#[test]
fn git_integration_deletion_preserves_stale_and_current_access_problems() {
    for (status, code) in [
        ("412 Precondition Failed", "precondition_failed"),
        ("403 Forbidden", "insufficient_permissions"),
        ("404 Not Found", "resource_not_found"),
        ("409 Conflict", "conflict"),
    ] {
        let body =
            json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"key-delete"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "deleteProjectGitIntegration",
                "--path",
                "projectId=pr_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--if-match",
                "\"old-key\"",
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn mcp_catalog_is_offline_and_needs_no_credential_or_valid_origin() {
    let out = command("not a URL", &["--api-key", "", "api", "mcp-tools"], None);
    let catalog = success(&out);
    assert_eq!(catalog["selection"], "discovery");
    assert_eq!(catalog["available_operation_count"], 315);
    let tools = catalog["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 8);
    assert!(
        tools
            .iter()
            .all(|tool| tool["annotations"]["readOnlyHint"] == true)
    );
    assert!(out.stderr.is_empty());
}

#[test]
fn mcp_catalog_filters_operations_without_dropping_conditional_schema() {
    let out = command(
        "not a URL",
        &[
            "api",
            "mcp-tools",
            "--operation",
            "getProject",
            "--operation",
            "setProjectKeySettings",
        ],
        None,
    );
    let catalog = success(&out);
    assert_eq!(catalog["selection"], "explicit");
    let tools = catalog["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    let replacement = tools
        .iter()
        .find(|tool| tool["_meta"]["lingohub/operationId"] == "setProjectKeySettings")
        .unwrap();
    assert_eq!(replacement["inputSchema"]["required"], json!(["path", "body"]));
    assert!(
        replacement["inputSchema"]["properties"]["headers"]["properties"]
            .get("If-Match")
            .is_some()
    );
    let tag = replacement["_meta"]["lingohub/tag"].as_str().unwrap();
    let selected = success(&command("not a URL", &["api", "mcp-tools", "--tag", tag], None));
    assert!(
        selected["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["_meta"]["lingohub/tag"] == tag)
    );
}

#[test]
fn mcp_permission_inputs_exclude_session_only_write_level() {
    let operations = [
        "setWorkspaceApiKeyWorkspacePermissions",
        "setWorkspaceApiKeyDefaultStackPermissions",
        "setWorkspaceApiKeyStackPermissions",
    ];
    let mut args = vec!["api", "mcp-tools"];
    for operation in operations {
        args.extend(["--operation", operation]);
    }
    let catalog = success(&command("not a URL", &args, None));
    let tools = catalog["tools"].as_array().unwrap();
    assert_eq!(tools.len(), operations.len());
    for tool in tools {
        let definitions = &tool["inputSchema"]["$defs"];
        assert_eq!(
            definitions["ApiKeyPermissionLevel"]["enum"],
            json!(["NO_ACCESS", "VIEW", "MANAGE"])
        );
        assert!(definitions.get("PermissionLevel").is_none());
    }
}

#[test]
fn large_escaped_key_round_trips_through_stdin_without_truncation() {
    let key = format!("\"{}\"", "\\\\".repeat(50000));
    let body = json!({"key":key,"group_key":null});
    let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "renameKey",
            "--path",
            "keyId=ress_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            "--file",
            "-",
        ],
        Some(&body.to_string()),
    );
    assert_eq!(success(&out), body);
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    let (_, payload) = requests[0].split_once("\r\n\r\n").unwrap();
    assert_eq!(serde_json::from_str::<Value>(payload).unwrap(), body);
}

#[test]
fn mcp_catalog_accounts_for_all_operations_and_reports_upload_adapters() {
    let catalog = success(&command("not a URL", &["api", "mcp-tools", "--all"], None));
    let tools = catalog["tools"].as_array().unwrap();
    let unsupported = catalog["unsupported"].as_array().unwrap();
    assert_eq!(tools.len() + unsupported.len(), 315);
    assert!(unsupported.iter().any(|op| op["operation_id"] == "createImport"));
    assert!(
        tools
            .windows(2)
            .all(|pair| pair[0]["name"].as_str() < pair[1]["name"].as_str())
    );
}

#[test]
fn mcp_catalog_rejects_unknown_and_conflicting_selectors_before_io() {
    for args in [
        vec!["api", "mcp-tools", "--operation", "missingOperation"],
        vec!["api", "mcp-tools", "--tag", "Missing tag"],
        vec!["api", "mcp-tools", "--all", "--operation", "getProject"],
        vec!["api", "mcp-tools", "--all", "--tag", "Projects"],
    ] {
        let out = command("not a URL", &args, None);
        assert_eq!(out.status.code(), Some(2));
        assert!(out.stdout.is_empty());
        assert!(!out.stderr.is_empty());
    }
}

#[test]
fn language_removal_forwards_optional_cleanup_choice_and_condition() {
    let branch = "prb_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let contract: Value = serde_json::from_str(include_str!("../contract/v2.json")).unwrap();
    let parameter =
        contract["paths"]["/v2/branches/{branchId}/languages/{languageCode}"]["delete"]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "delete_unused_stack_language")
            .unwrap();
    assert_eq!(parameter["schema"]["type"], "boolean");
    assert_eq!(parameter["schema"]["default"], true);
    assert_eq!(parameter["required"], false);
    for choice in [None, Some("true"), Some("false")] {
        let stub = Stub::new(vec![
            "HTTP/1.1 204 No Content\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".into(),
        ]);
        let branch_arg = format!("branchId={branch}");
        let query_arg = choice.map(|value| format!("delete_unused_stack_language={value}"));
        let mut args = vec![
            "api",
            "call",
            "deleteProjectLanguage",
            "--path",
            &branch_arg,
            "--path",
            "languageCode=de",
            "--if-match",
            "\"current-language\"",
        ];
        if let Some(query) = &query_arg {
            args.extend(["--query", query]);
        }
        let out = command(&stub.origin, &args, None);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(out.stdout.is_empty());
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        let target = requests[0]
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
        assert_eq!(url.path(), format!("/v2/branches/{branch}/languages/de"));
        assert_eq!(
            url.query_pairs().into_owned().collect::<Vec<_>>(),
            choice
                .map(|value| vec![("delete_unused_stack_language".to_owned(), value.to_owned())])
                .unwrap_or_default()
        );
        assert!(requests[0].starts_with("DELETE "));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("if-match: \"current-language\"")
        );
        assert!(requests[0].split_once("\r\n\r\n").unwrap().1.is_empty());
    }
}

#[test]
fn language_removal_preserves_strict_backend_query_errors_without_retry() {
    for values in [vec!["TRUE"], vec![""], vec!["false", "true"]] {
        let expected = json!({"status":400,"code":"invalid_parameter","request_id":"language-retention","errors":[{"parameter":"delete_unused_stack_language","code":"type","detail":"Expected a single boolean value: true or false."}]});
        let stub = Stub::new(vec![response("400 Bad Request", expected.clone(), "")]);
        let queries: Vec<_> = values
            .iter()
            .map(|v| format!("delete_unused_stack_language={v}"))
            .collect();
        let mut args = vec![
            "--json",
            "api",
            "call",
            "deleteProjectLanguage",
            "--path",
            "branchId=prb_0197c8f2-5a6b-7cde-8f12-3456789abcde",
            "--path",
            "languageCode=de",
        ];
        for query in &queries {
            args.extend(["--query", query]);
        }
        let out = command(&stub.origin, &args, None);
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            expected
        );
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        let target = requests[0]
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
        assert_eq!(
            url.query_pairs().map(|(_, v)| v.into_owned()).collect::<Vec<_>>(),
            values
        );
    }
}

#[test]
fn direct_sync_entry_reads_use_flat_public_ids_and_preserve_metadata() {
    for (operation, parameter, prefix, collection, object) in [
        (
            "getImportEntry",
            "importEntryId",
            "rie",
            "import-entries",
            "IMPORT_ENTRY",
        ),
        (
            "getExportEntry",
            "exportEntryId",
            "ree",
            "export-entries",
            "EXPORT_ENTRY",
        ),
    ] {
        let id = format!("{prefix}_0197c8f2-5a6b-7cde-8f12-3456789abcde");
        let mut body = json!({"object_type":object,"id":id,"status":"SUCCEEDED","resource_id":null,
            "file_size_bytes":120});
        if object == "EXPORT_ENTRY" {
            body["download_url"] = Value::Null;
            body["download_url_expires_at"] = Value::Null;
        }
        let stub = Stub::new(vec![response(
            "200 OK",
            body.clone(),
            "ETag: \"entry-current\"\r\n",
        )]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                operation,
                "--path",
                &format!("{parameter}={id}"),
                "--include-headers",
            ],
            None,
        );
        assert_eq!(success(&out), body);
        assert!(String::from_utf8_lossy(&out.stderr).contains("entry-current"));
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("GET /v2/{collection}/{id} ")));
        assert!(requests[0].split_once("\r\n\r\n").unwrap().1.is_empty());
    }
}

#[test]
fn direct_sync_entry_reads_preserve_current_access_errors_without_retry() {
    for (operation, parameter, prefix) in [
        ("getImportEntry", "importEntryId", "rie"),
        ("getExportEntry", "exportEntryId", "ree"),
    ] {
        for (status, code) in [
            ("401 Unauthorized", "invalid_api_key"),
            ("403 Forbidden", "insufficient_permissions"),
            ("404 Not Found", "resource_not_found"),
        ] {
            let expected =
                json!({"status":status[..3].parse::<u16>().unwrap(),"code":code,"request_id":"entry-read"});
            let stub = Stub::new(vec![response(status, expected.clone(), "")]);
            let out = command(
                &stub.origin,
                &[
                    "--json",
                    "api",
                    "call",
                    operation,
                    "--path",
                    &format!("{parameter}={prefix}_0197c8f2-5a6b-7cde-8f12-3456789abcde"),
                ],
                None,
            );
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            assert_eq!(
                serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
                expected
            );
            assert_eq!(stub.finish().len(), 1);
        }
    }
}

#[test]
fn import_inspection_preserves_multipart_and_advisory_results() {
    let file = UploadFixture::new("en.json", br#"{"hello":"World"}"#);
    let branch = "prb_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let body = json!({"object_type":"IMPORT_INSPECTION","branch_id":branch,"has_mixed_upload_types":false,
        "files":[{"path":"en.json","file_size_bytes":17,"upload_type":"PROJECT","is_extension_valid":true,
        "is_size_valid":true,"suggestions":{"action":"CREATE","suggested_paths":[],"language_codes":["en"]}}]});
    let stub = Stub::new(vec![response("200 OK", body.clone(), "ETag: \"inspection\"\r\n")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "upload",
            "inspectImportFiles",
            "--path",
            &format!("branchId={branch}"),
            "--upload",
            file.path(),
            "--include-headers",
        ],
        None,
    );
    assert_eq!(success(&out), body);
    assert!(String::from_utf8_lossy(&out.stderr).contains("inspection"));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!("POST /v2/branches/{branch}/imports/inspect ")));
    assert!(requests[0].contains("name=\"file\"; filename=\"en.json\""));
    assert!(requests[0].contains("{\"hello\":\"World\"}"));
    assert!(!requests[0].contains("idempotency-key:"));
}

#[test]
fn import_inspection_preserves_problems_without_retry() {
    for (status, code) in [
        ("403 Forbidden", "insufficient_permissions"),
        ("409 Conflict", "branch_not_ready"),
        ("422 Unprocessable Entity", "validation_failed"),
    ] {
        let file = UploadFixture::new("en.json", b"{}");
        let expected = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code});
        let stub = Stub::new(vec![response(status, expected.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "upload",
                "inspectImportFiles",
                "--path",
                "branchId=prb_0197c8f2-5a6b-7cde-8f12-3456789abcde",
                "--upload",
                file.path(),
            ],
            None,
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            expected
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn git_push_settings_read_and_replace_preserve_complete_value_and_condition() {
    let value = json!({
        "push_type":"PULL_REQUEST", "auto_push_default_branch_type":"PULL_REQUEST",
        "auto_push_other_branches_type":"NONE", "commit_message":"Translate {project}",
        "commit_description":"", "pull_request_branch":"translations/{base_branch}",
        "pull_request_title":"Update — Grüße", "pull_request_description":""
    });
    let stub = Stub::new(vec![
        response("200 OK", value.clone(), "ETag: \"current\"\r\n"),
        response("200 OK", value.clone(), ""),
    ]);
    let read = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "getProjectGitPushSettings",
            "--path",
            "projectId=pr_123",
            "--include-headers",
        ],
        None,
    );
    assert!(read.status.success(), "{}", String::from_utf8_lossy(&read.stderr));
    let write = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "setProjectGitPushSettings",
            "--file",
            "-",
            "--path",
            "projectId=pr_123",
            "--if-match",
            "\"current\"",
        ],
        Some(&value.to_string()),
    );
    assert!(
        write.status.success(),
        "{}",
        String::from_utf8_lossy(&write.stderr)
    );
    assert_eq!(serde_json::from_slice::<Value>(&write.stdout).unwrap(), value);
    let requests = stub.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("GET /v2/projects/pr_123/integrations/git/push-settings "));
    assert!(requests[1].starts_with("PUT /v2/projects/pr_123/integrations/git/push-settings "));
    let (headers, body) = requests[1].split_once("\r\n\r\n").unwrap();
    assert!(headers.to_lowercase().contains("if-match: \"current\""));
    assert_eq!(serde_json::from_str::<Value>(body).unwrap(), value);
}

#[test]
fn git_push_settings_preserve_validation_and_stale_problems_without_retry() {
    for (status, code, errors) in [
        (
            "422 Unprocessable Entity",
            "validation_failed",
            json!([{"code":"required","pointer":"#/commit_message","detail":"is required"}]),
        ),
        ("412 Precondition Failed", "precondition_failed", json!([])),
        ("403 Forbidden", "insufficient_permissions", json!([])),
        ("409 Conflict", "conflict", json!([])),
    ] {
        let body = json!({"status":status[..3].parse::<u16>().unwrap(), "code":code, "errors":errors, "request_id":"git-settings"});
        let stub = Stub::new(vec![response(status, body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "setProjectGitPushSettings",
                "--file",
                "-",
                "--path",
                "projectId=pr_123",
                "--if-match",
                "\"old\"",
            ],
            Some("{}"),
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap()["problem"],
            body
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn csv_structure_failure_preserves_terminal_parent_and_entry_reason() {
    let parent = json!({"object_type":"EXPORT","id":"reg_123","status":"FAILED","status_details":{"code":"ENTRY_FAILURES","detail":"One or more entries failed."}});
    let entries = page(
        json!([
            {"object_type":"EXPORT_ENTRY","id":"ree_123","status":"FAILED","status_details":{"code":"INVALID_FILE_STRUCTURE","detail":"Stored rows and cells do not form a valid file. Restore valid structure before exporting."}},
            {"object_type":"EXPORT_ENTRY","id":"ree_456","status":"SKIPPED","status_details":null}
        ]),
        None,
    );
    let api = Stub::new(vec![
        response("200 OK", parent.clone(), ""),
        response("200 OK", entries.clone(), ""),
    ]);
    let terminal = waiting(&api.origin, "getExport", "exportId=reg_123", &[]);
    assert_eq!(terminal.status.code(), Some(1));
    assert_eq!(serde_json::from_slice::<Value>(&terminal.stdout).unwrap(), parent);
    let details = command(
        &api.origin,
        &[
            "--json",
            "api",
            "call",
            "listExportEntries",
            "--path",
            "exportId=reg_123",
        ],
        None,
    );
    assert_eq!(success(&details), entries);
    let requests = api.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("GET /v2/exports/reg_123 "));
    assert!(requests[1].starts_with("GET /v2/exports/reg_123/entries "));
}

#[test]
fn import_cancellation_sends_no_body_and_preserves_validator_and_replay_key() {
    let id = "rin_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let metadata = json!({"object_type":"IMPORT","id":id,"branch_id":"prb_0197c8f2-5a6b-7cde-8f12-3456789abcde","type":"UPLOAD","status":"CANCELLED","status_details":null,"git_source":null,"segments_created":null,"segments_reactivated":null,"segments_deactivated":null,"segments_renamed":null,"segment_texts_updated":null,"created_at":"2026-10-02T10:00:00.000Z","updated_at":"2026-10-02T10:00:01.000Z"});
    let stub = Stub::new(vec![response(
        "200 OK",
        metadata.clone(),
        "ETag: \"cancelled\"\r\nIdempotent-Replayed: true\r\n",
    )]);
    let path = format!("importId={id}");
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "cancelImport",
            "--path",
            &path,
            "--if-match",
            "\"queued\"",
            "--idempotency-key",
            "cancel-once",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), metadata);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("etag: \"cancelled\""));
    assert!(stderr.contains("idempotent-replayed: true"));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with(&format!("POST /v2/imports/{id}/cancel HTTP/1.1")));
    let (headers, body) = requests[0].split_once("\r\n\r\n").unwrap();
    assert!(body.is_empty());
    let headers = headers.to_lowercase();
    assert!(headers.contains("if-match: \"queued\""));
    assert!(headers.contains("idempotency-key: cancel-once"));
}

#[test]
fn import_cancellation_preserves_claim_conflict_without_retrying() {
    let problem = json!({"type":"https://api.lingohub.com/problems/import_not_cancellable","title":"Import cannot be cancelled","status":409,"detail":"Worker has already claimed the import.","code":"import_not_cancellable","request_id":"test-cancel"});
    let stub = Stub::new(vec![response("409 Conflict", problem.clone(), "")]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "cancelImport",
            "--path",
            "importId=rin_0197c8f2-5a6b-7cde-8f12-3456789abcde",
        ],
        None,
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("import_not_cancellable"));
    assert_eq!(stub.finish().len(), 1);
}

#[test]
fn csv_rows_use_resource_listing_and_flat_row_identity() {
    let resource = "rc_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let row = "sgg_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let metadata = json!({"object_type":"CSV_ROW","id":row,"resource_id":resource,"position":2,"key":"row_2","cells":[{"object_type":"CSV_CELL","resource_segment_id":"ress_0197c8f2-5a6b-7cde-8f12-3456789abcde","segment_id":"seg_0197c8f2-5a6b-7cde-8f12-3456789abcde","position":1,"key":"cell_A2"}],"created_at":"2026-10-02T11:00:00.000Z","updated_at":"2026-10-02T11:00:00.000Z"});
    let page = json!({"object_type":"LIST","has_more":false,"next_cursor":null,"data":[metadata.clone()]});
    let stub = Stub::new(vec![
        response("200 OK", page.clone(), ""),
        response("200 OK", metadata.clone(), ""),
    ]);
    let listed = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listCsvRows",
            "--path",
            &format!("resourceId={resource}"),
            "--query",
            "limit=1",
            "--query",
            "cursor=opaque",
        ],
        None,
    );
    assert_eq!(success(&listed), page);
    let retrieved = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "getCsvRow",
            "--path",
            &format!("csvRowId={row}"),
        ],
        None,
    );
    assert_eq!(success(&retrieved), metadata);
    let requests = stub.finish();
    assert!(requests[0].starts_with(&format!("GET /v2/resources/{resource}/csv-rows?")));
    assert!(requests[0].contains("limit=1") && requests[0].contains("cursor=opaque"));
    assert!(requests[1].starts_with(&format!("GET /v2/csv-rows/{row} ")));
}

#[test]
fn membership_writes_preserve_merge_patch_nulls_replacement_bodies_and_conditional_headers() {
    let member = "wsu_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let stack = "stack_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let invitation = "svi_0197c8f2-5a6b-7cde-8f12-3456789abcde";
    let cases = [
        (
            "updateMember",
            "PATCH",
            format!("/v2/members/{member}"),
            vec![format!("memberId={member}")],
            json!({"workspace_role_id":"rol_0197c8f2-5a6b-7cde-8f12-3456789abcde"}),
        ),
        (
            "updateStackMember",
            "PATCH",
            format!("/v2/stacks/{stack}/members/{member}"),
            vec![format!("stackId={stack}"), format!("memberId={member}")],
            json!({"language_permissions":{"de":null,"fr":"READ_ACCESS"}}),
        ),
        (
            "setInvitationStackAssignment",
            "PUT",
            format!("/v2/invitations/{invitation}/stack-assignment"),
            vec![format!("invitationId={invitation}")],
            json!({"stack_role_id":"stackr_0197c8f2-5a6b-7cde-8f12-3456789abcde","stack_ids":[stack],"language_permissions":null}),
        ),
    ];
    for (operation, method, path, parameters, input) in cases {
        let expected = json!({"id":member,"updated_at":"2026-10-07T17:00:00.000Z"});
        let stub = Stub::new(vec![response("200 OK", expected.clone(), "")]);
        let mut args = vec![
            "--json",
            "api",
            "call",
            operation,
            "--if-match",
            "\"membership-current\"",
            "--file",
            "-",
        ];
        for parameter in &parameters {
            args.extend(["--path", parameter.as_str()]);
        }
        let out = command(&stub.origin, &args, Some(&input.to_string()));
        assert_eq!(success(&out), expected);
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("{method} {path} ")));
        assert!(
            requests[0]
                .to_lowercase()
                .contains("if-match: \"membership-current\"")
        );
        let sent: Value = serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(sent, input);
    }
}

#[test]
fn invitation_creation_preserves_metadata_location_replay_key_and_actionable_scope_errors() {
    let input = json!({"email":"alex@example.com","workspace_role_id":"rol_0197c8f2-5a6b-7cde-8f12-3456789abcde","message":"Welcome 👋","stack_assignment":null});
    let created = json!({"object_type":"INVITATION","id":"svi_0197c8f2-5a6b-7cde-8f12-3456789abcde","status":"PENDING"});
    let stub = Stub::new(vec![response(
        "201 Created",
        created.clone(),
        "Location: /v2/invitations/svi_0197c8f2-5a6b-7cde-8f12-3456789abcde\r\n",
    )]);
    let out = command(
        &stub.origin,
        &[
            "--json",
            "--include-headers",
            "api",
            "call",
            "createInvitation",
            "--idempotency-key",
            "invite-once",
            "--file",
            "-",
        ],
        Some(&input.to_string()),
    );
    assert_eq!(success(&out), created);
    assert!(String::from_utf8_lossy(&out.stderr).contains("location: /v2/invitations/"));
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("POST /v2/invitations "));
    assert!(
        requests[0]
            .to_lowercase()
            .contains("idempotency-key: invite-once")
    );
    let sent: Value = serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(sent, input);
    for (status, code) in [
        ("403 Forbidden", "insufficient_permissions"),
        ("409 Conflict", "owner_membership_protected"),
        ("409 Conflict", "conflict"),
        ("422 Unprocessable Entity", "validation_failed"),
    ] {
        let stub = Stub::new(vec![response(
            status,
            json!({"code":code,"title":"Invitation rejected","detail":"The invitation could not be created.","request_id":"invitation-create"}),
            "",
        )]);
        let out = command(
            &stub.origin,
            &["--json", "api", "call", "createInvitation", "--file", "-"],
            Some(&input.to_string()),
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(error.contains(code));
        assert!(error.contains("invitation-create"));
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn standalone_translation_keeps_partial_results_and_fails_without_retrying() {
    let input = json!({"source_language_code":"en","target_language_code":"de","texts":[{"key":"a","text":"Hello"},{"key":"b","text":"World"}]});
    for succeeded in [0, 1, 2] {
        let body = json!({"object_type":"LINA_TRANSLATE_RESULT","id":"lina_example","status":if succeeded == 2 {"SUCCEEDED"} else if succeeded == 1 {"PARTIALLY_SUCCEEDED"} else {"FAILED"},
            "failure":if succeeded == 2 {Value::Null} else {json!("EXECUTION_FAILED")},
            "results":[{"key":"a","is_translated":succeeded > 0,"text":if succeeded > 0 {json!("Hallo")} else {Value::Null}},
                {"key":"b","is_translated":succeeded > 1,"text":if succeeded > 1 {json!("Welt")} else {Value::Null}}]});
        let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "translateLina",
                "--path",
                "linaId=lina_example",
                "--file",
                "-",
            ],
            Some(&input.to_string()),
        );
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        assert_eq!(out.status.code(), Some(if succeeded == 2 { 0 } else { 1 }));
        if succeeded != 2 {
            let error: Value = serde_json::from_slice(&out.stderr).unwrap();
            assert_eq!(error["error"], "translation_incomplete");
            assert_eq!(error["succeeded"], succeeded);
            assert_eq!(error["failed"], 2 - succeeded);
        }
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn standalone_translation_rejects_missing_mismatched_or_inconsistent_outcomes() {
    let input =
        json!({"source_language_code":"en","target_language_code":"de","texts":[{"key":"a","text":"Hello"}]});
    for body in [
        json!({"object_type":"LINA_TRANSLATE_RESULT","status":"SUCCEEDED","failure":null,"results":[]}),
        json!({"object_type":"LINA_TRANSLATE_RESULT","status":"SUCCEEDED","failure":null,"results":[{"key":"wrong","is_translated":true,"text":"Hallo"}]}),
        json!({"object_type":"LINA_TRANSLATE_RESULT","status":"SUCCEEDED","failure":null,"results":[{"key":"a","is_translated":false,"text":null}]}),
        json!({"object_type":"LINA_TRANSLATE_RESULT","status":"SUCCEEDED","failure":null,"results":[{"key":"a","is_translated":true,"text":null}]}),
        json!({"object_type":"LINA_TRANSLATION","status":"SUCCEEDED","failure":null,"results":[{"key":"a","is_translated":true,"text":"Hallo"}]}),
        json!({"object_type":"FUTURE_RESULT","status":"SUCCEEDED","results":[{"key":"a","is_translated":true,"text":"Hallo"}]}),
    ] {
        let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "request",
                "POST",
                "/v2/linas/lina_example/translate",
                "--file",
                "-",
            ],
            Some(&input.to_string()),
        );
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        assert!(String::from_utf8_lossy(&out.stderr).contains("Malformed LINA_TRANSLATE_RESULT"));
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn lina_execution_polling_preserves_failed_run_and_never_retries_translation() {
    let pending = json!({"object_type":"LINA_EXECUTION","id":"tp_example","status":"QUEUED"});
    let failed = json!({"object_type":"LINA_EXECUTION","id":"tp_example","status":"FAILED","failure":"INCOMPLETE_TRANSLATION","selected_segment_count":2});
    let stub = Stub::new(vec![
        response("200 OK", pending, ""),
        response("200 OK", failed.clone(), ""),
    ]);
    let out = waiting(
        &stub.origin,
        "getLinaExecution",
        "executionId=tp_example",
        &["--interval", "1"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), failed);
    let requests = stub.finish();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|x| x.starts_with("GET /v2/lina-executions/tp_example "))
    );
}

#[test]
fn git_configuration_commands_preserve_conditions_jobs_and_provider_errors() {
    for (operation, suffix, input, output) in [
        (
            "setProjectGitSyncSettings",
            "sync-settings",
            json!({"import_mode":"PERIODIC", "import_schedule":{"period":"DAILY","time":"02:15"},"export_mode":"NONE"}),
            json!({"object_type":"PROJECT_GIT_SYNC_SETTINGS","import_mode":"PERIODIC"}),
        ),
        (
            "setProjectGitSelectionSettings",
            "selection-settings",
            json!({"is_branching":false,"path_filters":[],"branch_filters":[]}),
            json!({"object_type":"GIT_SELECTION_UPDATE","configuration":{"is_branching":false},"scheduled_imports":[{"id":"ri_job","status":"QUEUED"}]}),
        ),
        (
            "setProjectGitConnection",
            "connection",
            json!({"git_authentication_id":"iau_id","repository_url":"https://github.com/lingohub/example","git_branch":"main"}),
            json!({"object_type":"PROJECT_GIT_INTEGRATION","git_authentication_id":"iau_id"}),
        ),
    ] {
        let stub = Stub::new(vec![response("200 OK", output.clone(), "")]);
        let result = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                operation,
                "--path",
                "projectId=pr_id",
                "--if-match",
                "\"current\"",
                "--file",
                "-",
            ],
            Some(&input.to_string()),
        );
        assert_eq!(success(&result), output);
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("PUT /v2/projects/pr_id/integrations/git/{suffix} ")));
        let (headers, body) = requests[0].split_once("\r\n\r\n").unwrap();
        assert!(headers.to_lowercase().contains("if-match: \"current\""));
        assert_eq!(serde_json::from_str::<Value>(body).unwrap(), input);
    }
    for (status, code) in [
        ("502 Bad Gateway", "upstream_error"),
        ("504 Gateway Timeout", "upstream_timeout"),
        ("409 Conflict", "git_integration_unauthorized"),
    ] {
        let problem = json!({"status":status[..3].parse::<u16>().unwrap(),"code":code});
        let stub = Stub::new(vec![response(status, problem.clone(), "")]);
        let result = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "setProjectGitSelectionSettings",
                "--path",
                "projectId=pr_id",
                "--file",
                "-",
            ],
            Some("{}"),
        );
        assert_eq!(result.status.code(), Some(1));
        assert_eq!(
            serde_json::from_slice::<Value>(&result.stderr).unwrap()["problem"],
            problem
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn git_connection_creation_and_branch_disconnect_use_updated_contract() {
    for (operation, path, parameter) in [
        (
            "createProjectGitIntegration",
            "/v2/projects/pr_id/integrations/git",
            "projectId=pr_id",
        ),
        (
            "createBranchGitIntegration",
            "/v2/branches/prb_id/integrations/git",
            "branchId=prb_id",
        ),
    ] {
        let job = json!({"job_type":"IMPORT","data":{"id":"ri_job","status":"QUEUED"}});
        let stub = Stub::new(vec![response(
            "202 Accepted",
            job.clone(),
            "Location: /v2/imports/ri_job\r\n",
        )]);
        let result = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                operation,
                "--path",
                parameter,
                "--idempotency-key",
                "connect-once",
                "--include-headers",
                "--file",
                "-",
            ],
            Some("{}"),
        );
        assert_eq!(success(&result), job);
        assert!(String::from_utf8_lossy(&result.stderr).contains("/v2/imports/ri_job"));
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("POST {path} ")));
        assert!(
            requests[0]
                .to_lowercase()
                .contains("idempotency-key: connect-once")
        );
    }
    let stub = Stub::new(vec![
        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
    ]);
    let result = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "deleteBranchGitIntegration",
            "--path",
            "branchId=prb_id",
            "--if-match",
            "\"binding\"",
        ],
        None,
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("DELETE /v2/branches/prb_id/integrations/git "));
    assert!(requests[0].to_lowercase().contains("if-match: \"binding\""));
}

struct McpProcess {
    child: std::process::Child,
    replies: std::sync::mpsc::Receiver<String>,
    directory: std::path::PathBuf,
    next_id: u64,
    errors: Option<thread::JoinHandle<Vec<u8>>>,
}
impl McpProcess {
    fn new(origin: &str, selectors: &[&str]) -> Self {
        use std::io::BufRead;
        let directory = std::env::temp_dir().join(format!(
            "lh-mcp-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_lingohub"));
        command
            .env_clear()
            .current_dir(&directory)
            .env("LINGOHUB_API_KEY", "test-secret-do-not-print")
            .args(["--base-url", origin, "--color", "never", "api", "mcp-serve"])
            .args(selectors)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        command.env("SystemRoot", std::env::var_os("SystemRoot").unwrap());
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let errors = thread::spawn(move || {
            let mut captured = Vec::new();
            stderr.read_to_end(&mut captured).unwrap();
            captured
        });
        let (sender, replies) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                if sender.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        let mut process = Self {
            child,
            replies,
            directory,
            next_id: 1,
            errors: Some(errors),
        };
        let reply = process.call("initialize", json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"integration-test","version":"1"}}));
        assert_eq!(reply["result"]["serverInfo"]["name"], "lingohub-cli", "{reply}");
        process.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        process
    }
    fn send(&mut self, value: Value) {
        writeln!(self.child.stdin.as_mut().unwrap(), "{value}").unwrap();
    }
    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        let line = self
            .replies
            .recv_timeout(Duration::from_secs(15))
            .expect("MCP response within deadline");
        assert!(!line.contains("test-secret-do-not-print"));
        let response: Value = serde_json::from_str(&line).expect("stdout contains only protocol JSON");
        assert_eq!(response["id"], id, "{response}");
        response
    }
}
impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let errors = self.errors.take().unwrap().join().unwrap();
        if !thread::panicking() {
            assert!(!String::from_utf8_lossy(&errors).contains("test-secret-do-not-print"));
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn mcp_stdio_negotiates_calls_read_tools_and_rejects_unselected_mutations() {
    let body = json!({"object_type":"WORKSPACE","id":"space_id","name":"Grüße 日本語"});
    let stub = Stub::new(vec![response(
        "200 OK",
        body.clone(),
        "ETag: \"version\"\r\nX-Request-Id: req-mcp\r\n",
    )]);
    let mut mcp = McpProcess::new(&stub.origin, &[]);
    let catalog = mcp.call("tools/list", json!({}));
    let tools = catalog["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 8);
    assert!(
        tools
            .iter()
            .all(|tool| tool["annotations"]["readOnlyHint"] == true)
    );
    let name = tools
        .iter()
        .find(|tool| tool["_meta"]["lingohub/operationId"] == "getWorkspace")
        .unwrap()["name"]
        .as_str()
        .unwrap();
    let result = mcp.call("tools/call", json!({"name":name,"arguments":{}}));
    assert_eq!(result["result"]["isError"], false);
    assert_eq!(result["result"]["structuredContent"]["body"], body);
    assert_eq!(
        result["result"]["structuredContent"]["headers"]["etag"],
        "\"version\""
    );
    assert!(
        mcp.call(
            "tools/call",
            json!({"name":"projects.deleteProject","arguments":{}})
        )
        .get("error")
        .is_some()
    );
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET /v2/workspace "));
}

#[test]
fn mcp_stdio_preserves_mutation_condition_and_problem_without_retry() {
    let problem = json!({"status":412,"code":"precondition_failed","request_id":"mcp-write"});
    let stub = Stub::new(vec![response(
        "412 Precondition Failed",
        problem.clone(),
        "X-Request-Id: mcp-write\r\n",
    )]);
    let mut mcp = McpProcess::new(&stub.origin, &["--operation", "setProjectGitSyncSettings"]);
    let catalog = mcp.call("tools/list", json!({}));
    let name = catalog["result"]["tools"][0]["name"].as_str().unwrap();
    let input = json!({"import_mode":"NONE","export_mode":"NONE"});
    let result = mcp.call("tools/call", json!({"name":name,"arguments":{"path":{"projectId":"pr_id"},"body":input,"headers":{"If-Match":"\"stale\""}}}));
    assert_eq!(result["result"]["isError"], true);
    assert_eq!(result["result"]["structuredContent"]["problem"], problem);
    let requests = stub.finish();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("PUT /v2/projects/pr_id/integrations/git/sync-settings "));
    assert!(requests[0].to_lowercase().contains("if-match: \"stale\""));
    let payload = requests[0].split_once("\r\n\r\n").unwrap().1;
    assert_eq!(serde_json::from_str::<Value>(payload).unwrap(), input);
}

#[test]
fn mcp_stdio_retains_partial_translation_results_and_marks_them_incomplete() {
    let body = json!({"object_type":"LINA_TRANSLATE_RESULT","status":"PARTIALLY_SUCCEEDED","failure":"PROVIDER_ERROR", "results":[{"key":"one","text":"Hallo","is_translated":true},{"key":"two","text":null,"is_translated":false}]});
    let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
    let mut mcp = McpProcess::new(&stub.origin, &["--operation", "translateLina"]);
    let catalog = mcp.call("tools/list", json!({}));
    let name = catalog["result"]["tools"][0]["name"].as_str().unwrap();
    let result = mcp.call("tools/call", json!({"name":name,"arguments":{"path":{"linaId":"lina_id"},"body":{"source_language_code":"en","target_language_code":"de","texts":[{"key":"one","text":"Hello"},{"key":"two","text":"World"}]}}}));
    assert_eq!(result["result"]["isError"], true, "{result}");
    assert_eq!(result["result"]["structuredContent"]["body"], body);
    assert_eq!(
        result["result"]["structuredContent"]["outcome_error"]["error"],
        "translation_incomplete"
    );
    assert_eq!(stub.finish().len(), 1);
}

#[test]
fn mcp_stdio_pages_the_selected_catalog_and_rejects_unknown_argument_groups() {
    let mut mcp = McpProcess::new("http://127.0.0.1:1", &["--all"]);
    let mut names = std::collections::BTreeSet::new();
    let mut cursor = Value::Null;
    loop {
        let result = mcp.call(
            "tools/list",
            if cursor.is_null() {
                json!({})
            } else {
                json!({"cursor":cursor})
            },
        );
        for tool in result["result"]["tools"].as_array().unwrap() {
            assert!(names.insert(tool["name"].as_str().unwrap().to_owned()));
        }
        cursor = result["result"].get("nextCursor").cloned().unwrap_or(Value::Null);
        if cursor.is_null() {
            break;
        }
    }
    assert_eq!(names.len(), 312);
    assert!(
        mcp.call("tools/list", json!({"cursor":"invalid"}))
            .get("error")
            .is_some()
    );
    let workspace_tool = names.iter().find(|name| name.ends_with(".getWorkspace")).unwrap();
    let result = mcp.call(
        "tools/call",
        json!({"name":workspace_tool,"arguments":{"base_url":"https://elsewhere.test"}}),
    );
    assert_eq!(result["result"]["isError"], true, "{result}");
    assert!(result.to_string().contains("Unknown tool argument"), "{result}");
}

#[test]
fn content_provider_calls_preserve_selections_job_locations_and_conditions() {
    for (provider, slug) in [
        ("Storyblok", "storyblok"),
        ("Contentful", "contentful"),
        ("Zendesk", "zendesk"),
        ("HubSpot", "hubspot"),
        ("Stripo", "stripo"),
        ("CrispHelpdesk", "crisp-helpdesk"),
    ] {
        let input = json!({"resource_ids":["rc_example"], "language_codes":["de"]});
        let body =
            json!({"object_type":"JOB", "job_type":"EXPORT", "data":{"id":"rex_example", "status":"QUEUED"}});
        let stub = Stub::new(vec![response(
            "202 Accepted",
            body.clone(),
            "Location: /v2/exports/rex_example\r\n",
        )]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                &format!("push{provider}Integration"),
                "--path",
                "projectId=pr_example",
                "--file",
                "-",
                "--idempotency-key",
                "provider-push",
            ],
            Some(&input.to_string()),
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), body);
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("POST /v2/projects/pr_example/integrations/{slug}/push ")));
        assert!(
            requests[0]
                .to_lowercase()
                .contains("idempotency-key: provider-push")
        );
        assert_eq!(
            serde_json::from_str::<Value>(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap(),
            input
        );
    }
}

#[test]
fn content_selection_mcp_preserves_queued_work_and_stale_write_error() {
    let body = json!({"object_type":"CONTENT_SELECTION_UPDATE", "configuration":{"path_filters":[]}, "scheduled_imports":[{"id":"rim_example", "status":"QUEUED"}]});
    let problem = json!({"status":412, "code":"precondition_failed", "request_id":"content-selection"});
    let stub = Stub::new(vec![
        response("200 OK", body.clone(), ""),
        response("412 Precondition Failed", problem.clone(), ""),
    ]);
    let mut mcp = McpProcess::new(
        &stub.origin,
        &["--operation", "setProjectStoryblokSelectionSettings"],
    );
    let catalog = mcp.call("tools/list", json!({}));
    let tool = &catalog["result"]["tools"][0];
    assert_eq!(
        tool["inputSchema"]["$defs"]["SetContentSelectionSettings"]["properties"]["path_filters"]["type"],
        "array"
    );
    let arguments = json!({"path":{"projectId":"pr_example"},"body":{"path_filters":[]},"headers":{"If-Match":"\"selection-before\""}});
    let updated = mcp.call("tools/call", json!({"name":tool["name"], "arguments":arguments}));
    assert_eq!(updated["result"]["isError"], false);
    assert_eq!(updated["result"]["structuredContent"]["body"], body);
    let stale = mcp.call("tools/call", json!({"name":tool["name"], "arguments":arguments}));
    assert_eq!(stale["result"]["isError"], true);
    assert_eq!(stale["result"]["structuredContent"]["problem"], problem);
    let requests = stub.finish();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].starts_with("PUT /v2/projects/pr_example/integrations/storyblok/selection-settings ")
    );
    assert!(
        requests[0]
            .to_lowercase()
            .contains("if-match: \"selection-before\"")
    );
}

#[test]
fn content_project_creation_preserves_provider_setup_and_initial_import() {
    for (provider, slug) in [
        ("Storyblok", "storyblok"),
        ("Contentful", "contentful"),
        ("Zendesk", "zendesk"),
        ("HubSpot", "hubspot"),
        ("Stripo", "stripo"),
        ("CrispHelpdesk", "crisp-helpdesk"),
    ] {
        let input = json!({"name":"Provider project","source_language_code":"en","target_language_codes":["de"],"authentication_id":"iau_test"});
        let job = json!({"object_type":"JOB","job_type":"IMPORT","data":{"id":"rim_test","branch_id":"prb_test","status":"QUEUED"}});
        let stub = Stub::new(vec![response(
            "202 Accepted",
            job.clone(),
            "Location: /v2/imports/rim_test\r\n",
        )]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                &format!("create{provider}Project"),
                "--file",
                "-",
                "--idempotency-key",
                "create-content",
            ],
            Some(&input.to_string()),
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), job);
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("POST /v2/integrations/{slug}/projects ")));
        assert_eq!(
            serde_json::from_str::<Value>(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap(),
            input
        );
    }
}

#[test]
fn content_provider_authentication_creation_preserves_provider_input_and_returns_metadata_only() {
    for provider in [
        "STORYBLOK",
        "CONTENTFUL",
        "ZENDESK",
        "HUBSPOT",
        "STRIPO",
        "CRISP_HELPDESK",
    ] {
        let mut input =
            json!({"provider":provider,"name":"Internal content test","access_token":"fake-provider-token"});
        if provider == "STORYBLOK" {
            input["region"] = json!("EUROPE");
        }
        if provider == "ZENDESK" {
            input["provider_url"] = json!("https://example.zendesk.com");
            input["username"] = json!("agent@example.com");
        }
        if provider == "CRISP_HELPDESK" {
            input.as_object_mut().unwrap().remove("access_token");
            input["identifier"] = json!("plugin-identifier");
            input["key"] = json!("fake-provider-token");
            input["website_key"] = json!("website-key");
            input["signing_secret"] = Value::Null;
        }
        let metadata = json!({"object_type":"INTEGRATION_AUTHENTICATION","id":"iau_test","name":"Internal content test"});
        let stub = Stub::new(vec![response(
            "201 Created",
            metadata.clone(),
            "Location: /v2/integration-authentications/iau_test\r\n",
        )]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "createIntegrationAuthentication",
                "--file",
                "-",
                "--idempotency-key",
                "create-provider-auth",
            ],
            Some(&input.to_string()),
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), metadata);
        assert!(!String::from_utf8_lossy(&out.stderr).contains("fake-provider-token"));
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("POST /v2/integration-authentications "));
        assert_eq!(
            serde_json::from_str::<Value>(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap(),
            input
        );
    }
}

#[test]
fn authentication_lifecycle_preserves_conditional_headers_and_write_only_credentials() {
    for provider in [
        "STORYBLOK",
        "CONTENTFUL",
        "ZENDESK",
        "HUBSPOT",
        "STRIPO",
        "CRISP_HELPDESK",
    ] {
        let mut input = json!({"provider":provider,"access_token":"rotation-secret"});
        if provider == "STORYBLOK" {
            input["region"] = json!("EUROPE");
        }
        if provider == "ZENDESK" {
            input["provider_url"] = json!("https://example.zendesk.com");
            input["username"] = json!("agent@example.com");
        }
        if provider == "CRISP_HELPDESK" {
            input = json!({"provider":provider,"identifier":"plugin","key":"rotation-secret","website_key":"site","signing_secret":null});
        }
        let metadata =
            json!({"object_type":"INTEGRATION_AUTHENTICATION","id":"iau_test","name":"Build credentials"});
        let stub = Stub::new(vec![response("200 OK", metadata.clone(), "")]);
        let out = command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "setIntegrationAuthenticationCredentials",
                "--path",
                "integrationAuthenticationId=iau_test",
                "--if-match",
                "\"current\"",
                "--file",
                "-",
            ],
            Some(&input.to_string()),
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), metadata);
        assert!(!String::from_utf8_lossy(&out.stderr).contains("rotation-secret"));
        let requests = stub.finish();
        assert!(requests[0].starts_with("PUT /v2/integration-authentications/iau_test/credentials "));
        assert!(requests[0].to_lowercase().contains("if-match: \"current\""));
        assert_eq!(
            serde_json::from_str::<Value>(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap(),
            input
        );
    }
    let parameter = "integrationAuthenticationId=iau_test";
    let stub = Stub::new(vec![response(
        "200 OK",
        json!({"id":"iau_test","name":"Renamed"}),
        "",
    )]);
    let out = command(
        &stub.origin,
        &[
            "api",
            "call",
            "updateIntegrationAuthentication",
            "--path",
            parameter,
            "--if-match",
            "*",
            "--file",
            "-",
        ],
        Some(r#"{"name":"Renamed"}"#),
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(stub.finish()[0].starts_with("PATCH /v2/integration-authentications/iau_test "));
    let stub = Stub::new(vec![response("204 No Content", Value::Null, "")]);
    let out = command(
        &stub.origin,
        &[
            "api",
            "call",
            "deleteIntegrationAuthentication",
            "--path",
            parameter,
            "--if-match",
            "*",
        ],
        None,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(stub.finish()[0].starts_with("DELETE /v2/integration-authentications/iau_test "));
}

#[test]
fn contentful_discovery_preserves_selection_external_keys_and_opaque_cursors() {
    for (operation, path) in [
        ("listContentfulSpaces", "spaces"),
        ("listContentfulEnvironments", "environments"),
        ("listContentfulLanguages", "languages"),
    ] {
        let item = if path == "languages" {
            json!({"object_type":"CONTENTFUL_LANGUAGE", "code":"x-custom", "language_code":null, "is_source":false, "fallback_code":"en-US"})
        } else {
            json!({"key":"external-provider-key"})
        };
        let first = page(json!([item]), Some("opaque+/="));
        let last = page(json!([]), None);
        let stub = Stub::new(vec![
            response("200 OK", first.clone(), ""),
            response("200 OK", last.clone(), ""),
        ]);
        let mut args = vec![
            "--json",
            "api",
            "call",
            operation,
            "--path",
            "integrationAuthenticationId=iau_test",
            "--query",
            "limit=1",
        ];
        if path != "spaces" {
            args.extend(["--query", "space_key=space1"]);
        }
        if path == "languages" {
            args.extend(["--query", "environment_key=preview"]);
        }
        assert_eq!(success(&command(&stub.origin, &args, None)), first);
        args.extend(["--query", "cursor=opaque+/="]);
        assert_eq!(success(&command(&stub.origin, &args, None)), last);
        let requests = stub.finish();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert!(request.starts_with(&format!(
                "GET /v2/integration-authentications/iau_test/contentful-{path}?"
            )));
            if path != "spaces" {
                assert!(request.contains("space_key=space1"));
            }
            if path == "languages" {
                assert!(request.contains("environment_key=preview"));
            }
        }
        assert!(requests[1].contains("cursor=opaque%2B%2F%3D"));
    }
}

#[test]
fn hubspot_content_type_configuration_preserves_conditional_replacement() {
    let value = json!({"object_type":"HUBSPOT_CONTENT_TYPE_SETTINGS", "id":"pr_test", "enabled_content_types":["SITE_PAGE", "BLOG_POST"]});
    let stub = Stub::new(vec![
        response("200 OK", value.clone(), "ETag: \"categories\"\r\n"),
        response("200 OK", value.clone(), ""),
    ]);
    let result = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "getProjectHubSpotContentTypeSettings",
            "--path",
            "projectId=pr_test",
        ],
        None,
    );
    assert_eq!(success(&result), value);
    let body = r#"{"enabled_content_types":["SITE_PAGE","BLOG_POST"]}"#;
    let result = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "setProjectHubSpotContentTypeSettings",
            "--path",
            "projectId=pr_test",
            "--if-match",
            "\"categories\"",
            "--file",
            "-",
        ],
        Some(body),
    );
    assert_eq!(success(&result), value);
    let requests = stub.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("GET /v2/projects/pr_test/integrations/hubspot/content-type-settings "));
    assert!(requests[1].starts_with("PUT /v2/projects/pr_test/integrations/hubspot/content-type-settings "));
    assert!(requests[1].to_lowercase().contains("if-match: \"categories\""));
    assert_eq!(requests[1].split_once("\r\n\r\n").unwrap().1, body);
}

#[test]
fn manual_git_credentials_use_the_shared_route_and_parent_metadata_conditions() {
    for mut credentials in [
        json!({"provider":"AZURE", "access_token":"fake-token"}),
        json!({"provider":"BITBUCKET", "access_token":"fake-token"}),
        json!({"provider":"GITLAB", "access_token":"fake-token", "is_self_hosted":false}),
        json!({"provider":"AWS_CODE_COMMIT", "access_token":"fake-key:fake-secret", "region":"us-east-1"}),
    ] {
        let value = json!({"object_type":"INTEGRATION_AUTHENTICATION", "id":"iau_test", "name":"Manual setup", "type":"MANUAL", "status":"ACTIVE"});
        let stub = Stub::new(vec![
            response(
                "201 Created",
                value.clone(),
                "Location: /v2/integration-authentications/iau_test\r\n",
            ),
            response("200 OK", value.clone(), "ETag: \"manual\"\r\n"),
            response("200 OK", value.clone(), ""),
        ]);
        let replacement = credentials.to_string();
        credentials["name"] = json!("Manual setup");
        let creation = credentials.to_string();
        assert_eq!(
            success(&command(
                &stub.origin,
                &[
                    "--json",
                    "api",
                    "call",
                    "createIntegrationAuthentication",
                    "--file",
                    "-",
                    "--idempotency-key",
                    "manual-create"
                ],
                Some(&creation)
            )),
            value
        );
        assert_eq!(
            success(&command(
                &stub.origin,
                &[
                    "--json",
                    "api",
                    "call",
                    "getIntegrationAuthentication",
                    "--path",
                    "integrationAuthenticationId=iau_test"
                ],
                None
            )),
            value
        );
        assert_eq!(
            success(&command(
                &stub.origin,
                &[
                    "--json",
                    "api",
                    "call",
                    "setIntegrationAuthenticationCredentials",
                    "--path",
                    "integrationAuthenticationId=iau_test",
                    "--file",
                    "-",
                    "--if-match",
                    "\"manual\""
                ],
                Some(&replacement)
            )),
            value
        );
        let requests = stub.finish();
        assert!(requests[0].starts_with("POST /v2/integration-authentications "));
        assert!(
            requests[0]
                .to_lowercase()
                .contains("idempotency-key: manual-create")
        );
        assert_eq!(requests[0].split_once("\r\n\r\n").unwrap().1, creation);
        assert!(requests[1].starts_with("GET /v2/integration-authentications/iau_test "));
        assert!(requests[2].starts_with("PUT /v2/integration-authentications/iau_test/credentials "));
        assert!(requests[2].to_lowercase().contains("if-match: \"manual\""));
        assert_eq!(requests[2].split_once("\r\n\r\n").unwrap().1, replacement);
    }
}

#[test]
fn git_provider_discovery_preserves_namespace_and_pagination() {
    for (name, slug, collection) in [
        ("GitHub", "github", "repositories"),
        ("GitHub", "github", "branches"),
        ("GitLab", "gitlab", "repositories"),
        ("GitLab", "gitlab", "branches"),
        ("Bitbucket", "bitbucket", "workspaces"),
        ("Bitbucket", "bitbucket", "repositories"),
        ("Bitbucket", "bitbucket", "branches"),
    ] {
        let suffix = match collection {
            "repositories" => "Repositories",
            "branches" => "Branches",
            _ => "Workspaces",
        };
        let operation = format!("list{name}{suffix}");
        let first = page(
            json!([{"key":"company/group/app", "is_protected":null}]),
            Some("opaque+/="),
        );
        let last = page(json!([]), None);
        let stub = Stub::new(vec![
            response("200 OK", first.clone(), ""),
            response("200 OK", last.clone(), ""),
        ]);
        let mut args = vec![
            "--json",
            "api",
            "call",
            &operation,
            "--path",
            "integrationAuthenticationId=iau_test",
            "--query",
            "limit=1",
        ];
        if collection == "branches" {
            args.extend([
                "--query",
                "repository_namespace=company/group",
                "--query",
                "repository_name=app",
            ]);
        }
        if name == "Bitbucket" && collection == "repositories" {
            args.extend(["--query", "workspace_key=company"]);
        }
        assert_eq!(success(&command(&stub.origin, &args, None)), first);
        args.extend(["--query", "cursor=opaque+/="]);
        assert_eq!(success(&command(&stub.origin, &args, None)), last);
        let requests = stub.finish();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert!(request.starts_with(&format!(
                "GET /v2/integration-authentications/iau_test/{slug}-{collection}?"
            )));
            if collection == "branches" {
                assert!(request.contains("repository_namespace=company%2Fgroup"));
            }
            if name == "Bitbucket" && collection == "repositories" {
                assert!(request.contains("workspace_key=company"));
            }
        }
        assert!(requests[1].contains("cursor=opaque%2B%2F%3D"));
    }
}

#[test]
fn configuration_replacements_preserve_nested_bodies_and_conditions() {
    let payloads = [
        (
            "setStackAutomationSettings",
            "stackId=stack_id",
            "/v2/stacks/stack_id/automation-settings",
            json!({"rules":[]}),
        ),
        (
            "setProjectFormatSettings",
            "projectId=pr_id",
            "/v2/projects/pr_id/format-settings",
            json!({"configuration":{"file":{"general":{"charset":"UTF_8","file_name_languages":"ALL","is_source_used_when_missing":false,"comment_languages":"SOURCE","indentation":0,"is_final_newline_enabled":true},"json":{"is_array_indentation_enabled":true,"is_plural_order_alphabetical":false}}}}),
        ),
    ];
    for (operation, parameter, path, body) in payloads {
        let stub = Stub::new(vec![response(
            "200 OK",
            json!({"id":"pr_id","configuration":body}),
            "",
        )]);
        let result = command(
            &stub.origin,
            &[
                "api",
                "call",
                operation,
                "--path",
                parameter,
                "--file",
                "-",
                "--if-match",
                "\"current\"",
                "--json",
            ],
            Some(&body.to_string()),
        );
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("PUT {path} ")));
        assert!(requests[0].to_lowercase().contains("if-match: \"current\""));
        assert_eq!(
            serde_json::from_str::<Value>(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap(),
            body
        );
    }
}

#[test]
fn backup_schedule_replacement_preserves_branch_ids_and_conditional_headers() {
    let value = json!({"object_type":"BACKUP_SETTINGS", "id":"pr_test", "is_enabled":true,
        "schedule":{"period":"DAILY","time":"10:30","day_of_week":null},"branch_ids":["prb_test"]});
    let stub = Stub::new(vec![
        response("200 OK", value.clone(), "ETag: \"schedule\"\r\n"),
        response("200 OK", value.clone(), ""),
    ]);
    let current = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "getBackupSettings",
            "--path",
            "projectId=pr_test",
        ],
        None,
    );
    assert_eq!(success(&current), value);
    let body =
        r#"{"schedule":{"period":"DAILY","time":"10:30","day_of_week":null},"branch_ids":["prb_test"]}"#;
    let result = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "setBackupSettings",
            "--path",
            "projectId=pr_test",
            "--if-match",
            "\"schedule\"",
            "--file",
            "-",
        ],
        Some(body),
    );
    assert_eq!(success(&result), value);
    let requests = stub.finish();
    assert!(requests[0].starts_with("GET /v2/projects/pr_test/backup-settings "));
    assert!(requests[1].starts_with("PUT /v2/projects/pr_test/backup-settings "));
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("if-match: \"schedule\"")
    );
    assert_eq!(
        serde_json::from_str::<Value>(requests[1].split_once("\r\n\r\n").unwrap().1).unwrap(),
        serde_json::from_str::<Value>(body).unwrap()
    );
}

#[test]
fn native_provider_pages_preserve_large_pages_and_opaque_cursors_without_inventing_limit() {
    for (operation, collection) in [
        ("listCodeCommitRepositories", "repositories"),
        ("listCodeCommitBranches", "branches"),
    ] {
        let first = page(
            json!(
                (0..150)
                    .map(|i| json!({"key":format!("repo{i}"),"namespace":"","url":null,"commit_sha":null}))
                    .collect::<Vec<_>>()
            ),
            Some("opaque+/="),
        );
        let last = page(json!([]), None);
        let stub = Stub::new(vec![
            response("200 OK", first.clone(), ""),
            response("200 OK", last.clone(), ""),
        ]);
        let mut args = vec![
            "--json",
            "api",
            "call",
            operation,
            "--path",
            "integrationAuthenticationId=iau_test",
        ];
        if collection == "branches" {
            args.extend(["--query", "repository_name=app"]);
        }
        assert_eq!(success(&command(&stub.origin, &args, None)), first);
        args.extend(["--query", "cursor=opaque+/="]);
        assert_eq!(success(&command(&stub.origin, &args, None)), last);
        let requests = stub.finish();
        for request in &requests {
            assert!(request.starts_with(&format!(
                "GET /v2/integration-authentications/iau_test/codecommit-{collection}"
            )));
            assert!(!request.contains("limit="));
        }
        assert!(requests[1].contains("cursor=opaque%2B%2F%3D"));
    }
}

#[test]
fn complete_provider_collections_preserve_selection_without_pagination_parameters() {
    for (operation, provider, collection) in [
        ("listAzureRepositories", "azure", "repositories"),
        ("listAzureDirectoryEntries", "azure", "directory-entries"),
        ("listGitHubDirectoryEntries", "github", "directory-entries"),
        (
            "listCodeCommitDirectoryEntries",
            "codecommit",
            "directory-entries",
        ),
    ] {
        let result = page(
            json!([{"path":"locales/a b/en.json", "type":"OTHER", "possible_project_types":[]}]),
            None,
        );
        let stub = Stub::new(vec![response("200 OK", result.clone(), "")]);
        let mut args = vec![
            "--json",
            "api",
            "call",
            operation,
            "--path",
            "integrationAuthenticationId=iau_test",
        ];
        if collection == "repositories" {
            args.extend([
                "--query",
                "organization=company",
                "--query",
                "project_key=My Project",
            ]);
        } else {
            args.extend([
                "--query",
                "repository_name=app",
                "--query",
                "commit_sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "--query",
                "path=locales/a b",
            ]);
            if provider != "codecommit" {
                args.extend(["--query", "repository_namespace=company/My Project"]);
            }
        }
        assert_eq!(success(&command(&stub.origin, &args, None)), result);
        let requests = stub.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!(
            "GET /v2/integration-authentications/iau_test/{provider}-{collection}?"
        )));
        assert!(!requests[0].contains("limit="));
        assert!(!requests[0].contains("cursor="));
        if collection == "repositories" {
            assert!(requests[0].contains("project_key=My+Project"));
        } else {
            assert!(requests[0].contains("path=locales%2Fa+b"));
        }
    }
}

#[test]
fn provider_discovery_incomplete_problem_is_not_reported_as_an_empty_collection() {
    let problem = json!({"status":502,"code":"upstream_error","detail":"Provider discovery is incomplete or exceeds the response bound.","request_id":"discovery-bound"});
    let stub = Stub::new(vec![response("502 Bad Gateway", problem.clone(), "")]);
    let output = command(
        &stub.origin,
        &[
            "--json",
            "api",
            "call",
            "listAzureRepositories",
            "--path",
            "integrationAuthenticationId=iau_test",
            "--query",
            "organization=company",
            "--query",
            "project_key=App",
        ],
        None,
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("upstream_error"));
    assert!(stderr.contains("discovery-bound"));
    assert_eq!(stub.finish().len(), 1);
}

#[test]
fn discovery_selection_reads_azure_organizations_and_one_codecommit_branch_head() {
    let organizations = page(
        json!([{"object_type":"AZURE_ORGANIZATION","key":"external-key","name":"company"}]),
        None,
    );
    let branch = json!({"object_type":"GIT_PROVIDER_BRANCH","git_branch":"feature/one","commit_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","is_protected":null});
    let stub = Stub::new(vec![
        response("200 OK", organizations.clone(), ""),
        response("200 OK", branch.clone(), ""),
    ]);
    assert_eq!(
        success(&command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "listAzureOrganizations",
                "--path",
                "integrationAuthenticationId=iau_azure"
            ],
            None
        )),
        organizations
    );
    assert_eq!(
        success(&command(
            &stub.origin,
            &[
                "--json",
                "api",
                "call",
                "getCodeCommitBranch",
                "--path",
                "integrationAuthenticationId=iau_aws",
                "--query",
                "repository_name=app",
                "--query",
                "git_branch=feature/one"
            ],
            None
        )),
        branch
    );
    let requests = stub.finish();
    assert!(requests[0].starts_with("GET /v2/integration-authentications/iau_azure/azure-organizations "));
    assert!(requests[1].starts_with("GET /v2/integration-authentications/iau_aws/codecommit-branch?"));
    assert!(requests[1].contains("git_branch=feature%2Fone"));
    assert!(requests[1].contains("repository_name=app"));
    for request in requests {
        assert!(!request.contains("limit="));
        assert!(!request.contains("cursor="));
    }
}

#[test]
fn discovery_selection_preserves_access_and_provider_failures() {
    for operation in ["listAzureOrganizations", "getCodeCommitBranch"] {
        for (status, code) in [
            ("401 Unauthorized", "invalid_api_key"),
            ("403 Forbidden", "insufficient_permissions"),
            ("404 Not Found", "resource_not_found"),
            ("502 Bad Gateway", "upstream_error"),
        ] {
            let problem = json!({"title":"Discovery failed", "code":code, "detail":"Selection unavailable"});
            let stub = Stub::new(vec![response(status, problem.clone(), "")]);
            let mut args = vec![
                "--json",
                "api",
                "call",
                operation,
                "--path",
                "integrationAuthenticationId=iau_example",
            ];
            if operation == "getCodeCommitBranch" {
                args.extend(["--query", "repository_name=app", "--query", "git_branch=main"]);
            }
            let out = command(&stub.origin, &args, None);
            assert_eq!(out.status.code(), Some(1));
            let error: Value = serde_json::from_slice(&out.stderr).unwrap();
            assert_eq!(error["problem"], problem);
            assert_eq!(stub.finish().len(), 1);
        }
    }
}

#[test]
fn project_moves_preserve_intent_location_and_terminal_cancellation() {
    for (operation, action, input) in [
        (
            "moveProject",
            "move",
            Some("{\"stack_id\":\"stack_123\",\"is_merge_enabled\":false}"),
        ),
        ("detachProject", "detach", None),
    ] {
        let body = json!({"object_type":"JOB","job_type":"PROJECT_MOVE","data":{"object_type":"PROJECT_MOVE","id":"rsy_123","status":"QUEUED"}});
        let api = Stub::new(vec![response(
            "202 Accepted",
            body.clone(),
            "Location: /v2/project-moves/rsy_123\r\n",
        )]);
        let mut args = vec![
            "--json",
            "--include-headers",
            "--idempotency-key",
            "move-once",
            "api",
            "call",
            operation,
            "--path",
            "projectId=pr_123",
        ];
        if input.is_some() {
            args.extend(["--file", "-"]);
        }
        let out = command(&api.origin, &args, input);
        assert_eq!(success(&out), body);
        assert!(String::from_utf8_lossy(&out.stderr).contains("location: /v2/project-moves/rsy_123"));
        let requests = api.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!("POST /v2/projects/pr_123/{action} ")));
        assert!(requests[0].to_lowercase().contains("idempotency-key: move-once"));
        if let Some(input) = input {
            let sent: Value = serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(sent, serde_json::from_str::<Value>(input).unwrap());
        }
    }
    let cancelled = json!({"object_type":"PROJECT_MOVE","id":"rsy_123","status":"CANCELLED","status_details":{"code":"MOVE_SOURCE_CHANGED","detail":"Project moved before this request ran"}});
    let api = Stub::new(vec![response("200 OK", cancelled.clone(), "")]);
    let out = waiting(&api.origin, "getProjectMove", "projectMoveId=rsy_123", &[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(serde_json::from_slice::<Value>(&out.stdout).unwrap(), cancelled);
    assert_eq!(api.finish().len(), 1);
}

#[test]
fn remote_plaintext_origins_are_rejected_before_connecting() {
    for origin in [
        "http://example.com",
        "http://192.0.2.1",
        "http://[2001:db8::1]",
        "http://localhost.example.com",
    ] {
        let out = command(origin, &["--json", "workspace"], None);
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&out.stderr).contains("require HTTPS"));
    }
}

#[test]
fn resource_lists_reject_mutation_headers_and_json_calls_reject_multipart() {
    for args in [
        vec!["stack", "list", "--if-match", "*"],
        vec!["project", "list", "--idempotency-key", "key"],
        vec!["api", "call", "createImport", "--path", "branchId=prb_test"],
    ] {
        let out = command("http://127.0.0.1:1", &args, None);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn json_body_files_and_stdin_have_the_same_size_limit() {
    let body = " ".repeat(16 * 1024 * 1024 + 1);
    let file = UploadFixture::new("large-body.json", body.as_bytes());
    for source in [file.path(), "-"] {
        let out = command(
            "http://127.0.0.1:1",
            &["--json", "stack", "create", "--file", source],
            if source == "-" { Some(&body) } else { None },
        );
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&out.stderr).contains("16777216"));
    }
}

#[test]
fn all_pages_rejects_an_oversized_collection_instead_of_printing_partial_data() {
    let stub = Stub::new(vec![response("200 OK", page(json!(vec![0; 100_001]), None), "")]);
    let out = command(&stub.origin, &["--json", "stack", "list", "--all"], None);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("explicit cursors"));
    assert_eq!(stub.finish().len(), 1);
}

#[test]
fn all_pages_stops_endless_distinct_cursors_at_the_page_bound() {
    let replies = (0..1_000)
        .map(|i| response("200 OK", page(json!([]), Some(&format!("cursor-{i}"))), ""))
        .collect();
    let stub = Stub::new(replies);
    let out = command(&stub.origin, &["--json", "stack", "list", "--all"], None);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("1,000 pages"));
    assert_eq!(stub.finish().len(), 1_000);
}

#[test]
fn mcp_exclusive_bounds_are_numeric_in_offline_and_stdio_highlight_tools() {
    let operation = "attachContextImage";
    let catalog = success(&command(
        "http://127.0.0.1:1",
        &["api", "mcp-tools", "--operation", operation],
        None,
    ));
    let mut mcp = McpProcess::new("http://127.0.0.1:1", &["--operation", operation]);
    let reply = mcp.call("tools/list", json!({}));
    for tool in [&catalog["tools"][0], &reply["result"]["tools"][0]] {
        let fields = &tool["inputSchema"]["$defs"]["SetContextImageHighlightBox"]["properties"];
        for field in ["width", "height"] {
            assert_eq!(fields[field]["exclusiveMinimum"], json!(0));
            assert!(fields[field].get("minimum").is_none());
        }
        for field in ["left", "top"] {
            assert_eq!(fields[field]["minimum"], json!(0));
            assert!(fields[field].get("exclusiveMinimum").is_none());
        }
        for field in ["left", "top", "width", "height"] {
            assert_eq!(fields[field]["maximum"], json!(1));
            assert!(fields[field].get("exclusiveMaximum").is_none());
        }
    }
}

#[test]
fn trusted_configuration_ignores_repository_dotenv_and_honors_explicit_precedence() {
    for source in ["home", "environment", "flag", "userprofile"] {
        let home = UploadFixture::new(".lingohub", b"");
        let trusted = Stub::new(vec![response("200 OK", json!({"id":"trusted-workspace"}), "")]);
        let trap = Stub::new(vec![]);
        let repository = home.0.with_file_name("repository");
        let nested = repository.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            repository.join(".env"),
            format!(
                "LINGOHUB_BASE_URL={}\nLINGOHUB_API_KEY=foreign-workspace-key\n",
                trap.origin
            ),
        )
        .unwrap();
        let home_origin = if source == "environment" || source == "flag" {
            &trap.origin
        } else {
            &trusted.origin
        };
        std::fs::write(
            &home.0,
            format!("LINGOHUB_API_KEY=test-secret-do-not-print\nLINGOHUB_BASE_URL={home_origin}\n"),
        )
        .unwrap();
        let mut process = Command::new(env!("CARGO_BIN_EXE_lingohub"));
        process
            .env_clear()
            .current_dir(&nested)
            .args(["--json", "workspace"]);
        process.env(
            if source == "userprofile" {
                "USERPROFILE"
            } else {
                "HOME"
            },
            home.0.parent().unwrap(),
        );
        #[cfg(windows)]
        process.env("SystemRoot", std::env::var_os("SystemRoot").unwrap());
        if source == "environment" {
            process.env("LINGOHUB_BASE_URL", &trusted.origin);
        } else if source == "flag" {
            process
                .env("LINGOHUB_BASE_URL", &trap.origin)
                .args(["--base-url", &trusted.origin]);
        }
        let out = process.output().unwrap();
        assert_eq!(success(&out)["id"], "trusted-workspace");
        assert!(!String::from_utf8_lossy(&out.stderr).contains("test-secret-do-not-print"));
        let requests = trusted.finish();
        assert_eq!(
            header_values(&requests[0], "authorization"),
            ["Bearer test-secret-do-not-print"]
        );
        trap.finish();
    }
}

#[test]
fn commands_reject_unsupported_request_flags_before_network_access() {
    let trap = Stub::new(vec![]);
    for args in [
        vec!["workspace"],
        vec!["api", "operations"],
        vec!["api", "mcp-tools"],
        vec!["completion"],
        vec!["tui"],
    ] {
        for flag in ["--if-match", "--idempotency-key", "--file"] {
            let mut input = args.clone();
            input.extend([flag, "unused"]);
            let out = command(&trap.origin, &input, None);
            assert_eq!(out.status.code(), Some(2));
            assert_error_contains(&out, "does not accept");
        }
    }
    trap.finish();
}

#[test]
fn export_metadata_requires_http_200_before_any_artifact_request() {
    let trap = Stub::new(vec![]);
    for status in ["201 Created", "202 Accepted"] {
        for entries in [false, true] {
            let fixture = UploadFixture::new("marker", b"");
            let url = json!(format!("{}/artifact", trap.origin));
            let metadata = if entries {
                entry_page(json!([entry_metadata("SUCCEEDED", url)]), Value::Null)
            } else {
                export_metadata("SUCCEEDED", url)
            };
            let api = Stub::new(vec![response(status, metadata, "")]);
            let destination = fixture.0.with_file_name("download.zip");
            let out = if entries {
                entry_download(&api.origin, &destination, &[])
            } else {
                download(&api.origin, &destination, &[])
            };
            assert_error_contains(&out, "Expected HTTP 200");
            api.finish();
            assert!(!destination.exists());
        }
    }
    trap.finish();
}

#[test]
fn export_artifacts_support_localhost_without_sending_api_credentials() {
    let artifact = Stub::new(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into(),
    ]);
    let api = Stub::new(vec![response(
        "200 OK",
        export_metadata(
            "SUCCEEDED",
            json!(format!(
                "{}/export",
                artifact.origin.replace("127.0.0.1", "localhost")
            )),
        ),
        "",
    )]);
    let fixture = UploadFixture::new("marker", b"");
    let destination = fixture.0.with_file_name("download.zip");
    success(&download(&api.origin, &destination, &[]));
    assert_eq!(std::fs::read(destination).unwrap(), b"ok");
    assert!(header_values(&artifact.finish()[0], "authorization").is_empty());
    api.finish();
}

#[test]
fn human_resource_views_restore_labels_alignment_placeholders_and_real_page_counts() {
    for (kind, command_name, id, title) in [
        ("STACK", "stack", "stack_test", "Stacks"),
        ("PROJECT", "project", "pr_test", "Projects"),
        ("STYLE_GUIDE", "style-guide", "sg_test", "Style Guides"),
    ] {
        let body = json!({"object_type":kind,"id":id,"name":"Example","description":null});
        let stub = Stub::new(vec![response("200 OK", page(json!([body]), Some("next")), "")]);
        let out = command(&stub.origin, &[command_name, "list"], None);
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.contains(&format!("{title} (1 shown)")), "{text}");
        assert!(text.contains(&format!("ID           {id}")), "{text}");
        assert!(text.contains("Name         Example"), "{text}");
        assert!(text.contains("Description  —"), "{text}");
        assert!(!text.contains("object_type"));
        assert!(!text.contains("total"));
        assert!(!text.contains("page 1/"));
        assert!(!text.contains('\u{1b}'));
        assert!(String::from_utf8_lossy(&out.stderr).contains("next_cursor: next"));
        stub.finish();
    }
}

#[test]
fn resource_colors_respect_policy_and_json_keeps_every_wire_field() {
    let body = json!({"object_type":"WORKSPACE","id":"space_test","name":"Example","last_active_at":null});
    for (color, json_mode, colored) in [
        ("always", false, true),
        ("never", false, false),
        ("auto", false, false),
        ("always", true, false),
    ] {
        let stub = Stub::new(vec![response("200 OK", body.clone(), "")]);
        let mut args = vec!["--color", color, "workspace"];
        if json_mode {
            args.push("--json");
        }
        let out = command(&stub.origin, &args, None);
        assert!(out.status.success());
        assert_eq!(out.stdout.contains(&0x1b), colored);
        if json_mode {
            assert_eq!(success(&out), body);
        } else {
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(text.contains("Last Active At"));
            assert!(text.contains('—'));
        }
        stub.finish();
    }
    let trap = Stub::new(vec![]);
    for (color, colored) in [("always", true), ("never", false), ("auto", false)] {
        let out = command(
            &trap.origin,
            &["--color", color, "project", "get", "invalid/id"],
            None,
        );
        assert_eq!(out.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&out.stderr).contains("Error:"));
        assert_eq!(out.stderr.contains(&0x1b), colored);
    }
    trap.finish();
}

const RELEASE_PATH: &str = "GET /repos/lingohub/lingohub-cli/releases/latest ";

fn release(tag: &str) -> String {
    response("200 OK", json!({"tag_name": tag, "name": tag, "assets": []}), "")
}

fn installer(script: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{script}",
        script.len()
    )
}

/// Runs the CLI without an API key against a loopback release host.
fn upgrade_command(releases: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
    upgrade_command_at(
        std::path::Path::new(env!("CARGO_BIN_EXE_lingohub")),
        releases,
        args,
        env,
    )
}

fn upgrade_command_at(
    binary: &std::path::Path,
    releases: &str,
    args: &[&str],
    env: &[(&str, &str)],
) -> Output {
    let dir = std::env::temp_dir().join(format!(
        "lingohub-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut command = Command::new(binary);
    command
        .env_clear()
        .current_dir(&dir)
        .env("HOME", &dir)
        .env("LINGOHUB_RELEASES_URL", releases)
        .envs(env.iter().copied())
        .args(["--color", "never"])
        .args(args)
        .stdin(Stdio::null());
    #[cfg(unix)]
    command.env("PATH", "/usr/bin:/bin");
    #[cfg(windows)]
    command
        .env(
            "SystemRoot",
            std::env::var_os("SystemRoot").expect("Windows SystemRoot"),
        )
        .env("TEMP", std::env::temp_dir())
        .env("TMP", std::env::temp_dir());
    let result = command.output().unwrap();
    let _ = std::fs::remove_dir_all(dir);
    result
}

/// A copy of the CLI in a directory of its own, which an upgrade test may replace.
fn disposable_binary() -> (std::path::PathBuf, std::path::PathBuf) {
    let built = std::path::Path::new(env!("CARGO_BIN_EXE_lingohub"));
    let dir = std::env::temp_dir().join(format!(
        "lingohub-install-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    // Unix resolves symlinked temp directories (macOS /var → /private/var) the way the CLI
    // resolves its own path. Windows canonical paths carry a \\?\ prefix the CLI never shows.
    #[cfg(unix)]
    let dir = dir.canonicalize().unwrap();
    let binary = dir.join(built.file_name().unwrap());
    std::fs::copy(built, &binary).unwrap();
    (dir, binary)
}

fn reported_version(binary: &std::path::Path) -> String {
    let output = Command::new(binary).arg("--version").output().unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn upgrade_check_reports_a_newer_release_without_an_api_key() {
    let stub = Stub::new(vec![release("v99.0.0")]);
    let out = upgrade_command(&stub.origin, &["upgrade", "--check", "--json"], &[]);
    assert_eq!(
        success(&out),
        json!({
            "installed_version": env!("CARGO_PKG_VERSION"),
            "latest_version": "99.0.0",
            "update_available": true,
            "installation": "standalone",
            "upgraded": false,
        })
    );
    let requests = stub.finish();
    assert!(requests[0].starts_with(RELEASE_PATH), "{}", requests[0]);
    assert_eq!(
        header_values(&requests[0], "Accept"),
        ["application/vnd.github+json"]
    );
    assert_eq!(
        header_values(&requests[0], "User-Agent"),
        [concat!("lingohub-cli/", env!("CARGO_PKG_VERSION"))]
    );
    assert!(header_values(&requests[0], "Authorization").is_empty());

    let stub = Stub::new(vec![release("v99.0.0")]);
    let out = upgrade_command(&stub.origin, &["upgrade", "--check"], &[]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!(
            "lingohub 99.0.0 is available (installed: {}). Upgrade with: lingohub upgrade\n",
            env!("CARGO_PKG_VERSION")
        )
    );
    stub.finish();
}

#[test]
fn upgrade_reports_a_current_installation_without_downloading() {
    let current = format!("v{}", env!("CARGO_PKG_VERSION"));
    for args in [&["upgrade"][..], &["upgrade", "--check"]] {
        let stub = Stub::new(vec![release(&current)]);
        let out = upgrade_command(&stub.origin, args, &[]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("lingohub {} is the latest version.\n", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(stub.finish().len(), 1);
    }
}

#[test]
fn upgrade_reports_missing_or_invalid_releases() {
    for (reply, expected) in [
        (
            response("404 Not Found", json!({"message": "Not Found"}), ""),
            "No published lingohub release was found",
        ),
        (
            response("403 Forbidden", json!({"message": "API rate limit exceeded"}), ""),
            "GitHub refused the release lookup (rate limit); try again later",
        ),
        (release("nightly"), "The latest release has no valid version tag"),
        (
            release("v1.0.0-\u{1b}[31mred"),
            "The latest release has no valid version tag",
        ),
    ] {
        let stub = Stub::new(vec![reply]);
        let out = upgrade_command(&stub.origin, &["upgrade", "--json"], &[]);
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stderr).unwrap(),
            json!({"error": expected})
        );
        stub.finish();
    }
}

#[test]
fn upgrade_only_accepts_a_loopback_release_override_and_no_request_flags() {
    let out = upgrade_command("https://example.com", &["upgrade", "--check"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert_error_contains(&out, "LINGOHUB_RELEASES_URL must be a loopback origin");

    let trap = Stub::new(vec![]);
    for flags in [
        &["--field", "name=\"x\""][..],
        &["--if-match", "\"etag\""],
        &["--include-headers"],
    ] {
        let mut args = vec!["upgrade"];
        args.extend_from_slice(flags);
        let out = upgrade_command(&trap.origin, &args, &[]);
        assert_eq!(out.status.code(), Some(2), "{flags:?}");
    }
    trap.finish();
}

#[cfg(unix)]
#[test]
fn upgrade_runs_the_release_installer_into_the_executable_directory() {
    let (dir, binary) = disposable_binary();
    let marker = dir.join("marker");
    // Like the real installer, stage the new binary and move it over the running one.
    let script = r#"printf '%s\n%s\n' "$LINGOHUB_INSTALL_DIR" "$LINGOHUB_NO_MODIFY_PATH" > "$LINGOHUB_TEST_MARKER"
printf '#!/bin/sh\necho "lingohub 99.0.0"\n' > "$LINGOHUB_INSTALL_DIR/.lingohub.new"
chmod +x "$LINGOHUB_INSTALL_DIR/.lingohub.new"
mv "$LINGOHUB_INSTALL_DIR/.lingohub.new" "$LINGOHUB_INSTALL_DIR/lingohub"
echo installer-progress
"#;
    let stub = Stub::new(vec![release("v99.0.0"), installer(script)]);
    let out = upgrade_command_at(
        &binary,
        &stub.origin,
        &["upgrade", "--json"],
        &[("LINGOHUB_TEST_MARKER", marker.to_str().unwrap())],
    );
    assert_eq!(
        success(&out),
        json!({
            "installed_version": env!("CARGO_PKG_VERSION"),
            "latest_version": "99.0.0",
            "update_available": false,
            "installation": "standalone",
            "upgraded": true,
        })
    );
    // Installer progress never mixes into the JSON result on stdout.
    assert!(String::from_utf8_lossy(&out.stderr).contains("installer-progress"));
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        format!("{}\n1\n", dir.display()),
        "installs in place without touching PATH"
    );
    assert_eq!(reported_version(&binary), "lingohub 99.0.0");
    let requests = stub.finish();
    assert!(requests[0].starts_with(RELEASE_PATH));
    assert!(
        requests[1]
            .starts_with("GET /lingohub/lingohub-cli/releases/download/v99.0.0/lingohub-installer.sh ")
    );
    assert!(header_values(&requests[1], "Authorization").is_empty());
    let mut left = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    left.sort();
    assert_eq!(left, ["lingohub", "marker"], "the writability probe is removed");
    std::fs::remove_dir_all(dir).unwrap();
}

/// A fake installer that stages a binary reporting `version`, moves it over the installed one
/// like the real installer, and exits with `exit_code`.
#[cfg(unix)]
fn replacing_installer(version: &str, exit_code: u8) -> String {
    format!(
        r#"printf '#!/bin/sh\necho "lingohub {version}"\n' > "$LINGOHUB_INSTALL_DIR/.lingohub.new"
chmod +x "$LINGOHUB_INSTALL_DIR/.lingohub.new"
mv "$LINGOHUB_INSTALL_DIR/.lingohub.new" "$LINGOHUB_INSTALL_DIR/lingohub"
exit {exit_code}
"#
    )
}

#[cfg(unix)]
#[test]
fn upgrade_leaves_the_installation_unchanged_when_the_installer_fails() {
    let unchanged = "; the installed version is unchanged";
    for (script, problem) in [
        // Fails before writing anything.
        ("echo broken >&2\nexit 3\n".to_owned(), None),
        // Fails after replacing the binary.
        (replacing_installer("99.0.0", 4), None),
        // Succeeds without writing the binary.
        ("echo installed elsewhere\n".to_owned(), Some("no version")),
        // Succeeds but installs another version.
        (replacing_installer("98.0.0", 0), Some("98.0.0")),
    ] {
        let (dir, binary) = disposable_binary();
        let stub = Stub::new(vec![release("v99.0.0"), installer(&script)]);
        let out = upgrade_command_at(&binary, &stub.origin, &["upgrade"], &[]);
        assert_eq!(out.status.code(), Some(1), "{script}");
        assert!(out.stdout.is_empty());
        let expected = match problem {
            None => format!("The installer failed{unchanged}"),
            Some(reported) => format!(
                "The installer finished, but {} reported {reported} instead of 99.0.0{unchanged}",
                binary.display()
            ),
        };
        assert_error_contains(&out, &expected);
        stub.finish();
        assert_eq!(
            reported_version(&binary),
            concat!("lingohub ", env!("CARGO_PKG_VERSION")),
            "the previous binary is back: {script}"
        );
        let left = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(left, ["lingohub"], "no backup or probe is left behind");
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(windows)]
#[test]
fn upgrade_restores_the_windows_executable_when_the_installer_fails() {
    let (dir, binary) = disposable_binary();
    let stub = Stub::new(vec![release("v99.0.0"), installer("exit 3\r\n")]);
    let out = upgrade_command_at(&binary, &stub.origin, &["upgrade"], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert_error_contains(&out, "The installer failed; the installed version is unchanged");
    let requests = stub.finish();
    assert!(
        requests[1]
            .starts_with("GET /lingohub/lingohub-cli/releases/download/v99.0.0/lingohub-installer.ps1 ")
    );
    // The running executable was moved aside for the installer and is back in place.
    assert!(!dir.join("lingohub.exe.previous").exists());
    assert_eq!(
        reported_version(&binary),
        concat!("lingohub ", env!("CARGO_PKG_VERSION"))
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(windows)]
#[test]
fn upgrade_restores_the_windows_executable_when_another_version_was_installed() {
    let (dir, binary) = disposable_binary();
    // Mark the copy to tell it apart from the build; Windows ignores bytes after the image.
    std::fs::OpenOptions::new()
        .append(true)
        .open(&binary)
        .unwrap()
        .write_all(b"lingohub-test-original")
        .unwrap();
    // Installs the unmarked build, which runs but reports the current version, not 99.0.0.
    let script = "[System.IO.File]::Copy($env:LINGOHUB_TEST_BUILD, [System.IO.Path]::Combine($env:LINGOHUB_INSTALL_DIR, 'lingohub.exe'), $true)\r\n";
    let stub = Stub::new(vec![release("v99.0.0"), installer(script)]);
    let out = upgrade_command_at(
        &binary,
        &stub.origin,
        &["upgrade"],
        &[("LINGOHUB_TEST_BUILD", env!("CARGO_BIN_EXE_lingohub"))],
    );
    assert_eq!(out.status.code(), Some(1));
    assert_error_contains(
        &out,
        &format!(
            "The installer finished, but {} reported {} instead of 99.0.0; the installed version is unchanged",
            binary.display(),
            env!("CARGO_PKG_VERSION")
        ),
    );
    stub.finish();
    assert!(
        std::fs::read(&binary)
            .unwrap()
            .ends_with(b"lingohub-test-original"),
        "the previous executable is back, not the one the installer wrote"
    );
    assert!(!dir.join("lingohub.exe.previous").exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn non_interactive_commands_never_look_up_releases() {
    let api = Stub::new(vec![
        response("200 OK", json!({"object_type":"WORKSPACE","id":"space_test"}), ""),
        response("200 OK", json!({"object_type":"WORKSPACE","id":"space_test"}), ""),
    ]);
    let releases = Stub::new(vec![]);
    for args in [&["workspace"][..], &["workspace", "--json"]] {
        let dir = std::env::temp_dir().join(format!(
            "lingohub-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_lingohub"));
        command
            .env_clear()
            .current_dir(&dir)
            .env("HOME", &dir)
            .env("LINGOHUB_API_KEY", "test-secret-do-not-print")
            .env("LINGOHUB_RELEASES_URL", &releases.origin)
            .args(["--base-url", &api.origin, "--color", "never"])
            .args(args);
        #[cfg(windows)]
        command.env(
            "SystemRoot",
            std::env::var_os("SystemRoot").expect("Windows SystemRoot"),
        );
        let out = command.output().unwrap();
        let _ = std::fs::remove_dir_all(dir);
        assert!(out.status.success());
        assert!(!String::from_utf8_lossy(&out.stderr).contains("is available"));
    }
    api.finish();
    releases.finish();
}

const LOGIN_TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJjYXRfMSJ9.c2lnbmF0dXJl";
const LOGIN_SECRET: &str = "login-secret-do-not-print";
const ISSUED_KEY: &str = "lh-api_eyJhbGciOiJIUzI1NiJ9.issued-key-do-not-print";
const VERIFICATION_URL: &str =
    "https://app.lingohub.com/?authentication-token=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJjYXRfMSJ9.c2lnbmF0dXJl";
const ISSUED_KEY_NAME: &str = "LingoHub CLI on Test Laptop (2026-11-04 21:05:33 UTC)";

/// A home directory holding the user configuration of one CLI run.
struct Home(std::path::PathBuf);
impl Home {
    fn new(config: Option<&str>) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "lingohub-home-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // The CLI reports the path it was given; resolve macOS /var → /private/var up front.
        #[cfg(unix)]
        let dir = dir.canonicalize().unwrap();
        if let Some(config) = config {
            std::fs::write(dir.join(".lingohub"), config).unwrap();
        }
        Self(dir)
    }
    fn config(&self) -> std::path::PathBuf {
        self.0.join(".lingohub")
    }
    fn read(&self) -> Option<String> {
        std::fs::read_to_string(self.config()).ok()
    }
    fn entries(&self) -> Vec<String> {
        let mut entries = std::fs::read_dir(&self.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Runs the CLI like a user without LINGOHUB_API_KEY whose home directory is `home`.
fn login_command(origin: &str, home: &Home, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lingohub"));
    command
        .env_clear()
        .current_dir(&home.0)
        .env("HOME", &home.0)
        .envs(env.iter().copied())
        .args(["--base-url", origin, "--color", "never"])
        .args(args)
        .stdin(Stdio::null());
    #[cfg(windows)]
    command.env(
        "SystemRoot",
        std::env::var_os("SystemRoot").expect("Windows SystemRoot"),
    );
    let out = command.output().unwrap();
    for secret in [LOGIN_SECRET, ISSUED_KEY] {
        assert!(!String::from_utf8_lossy(&out.stdout).contains(secret));
        assert!(!String::from_utf8_lossy(&out.stderr).contains(secret));
    }
    out
}

fn login_started(expires_in_seconds: u64, poll_interval_seconds: u64) -> String {
    response(
        "200 OK",
        json!({
            "clientAuthenticationToken": LOGIN_TOKEN,
            "secret": LOGIN_SECRET,
            "verificationCode": "WDJB-MJHT",
            "verificationUrl": VERIFICATION_URL,
            "expiresInSeconds": expires_in_seconds,
            "pollIntervalSeconds": poll_interval_seconds,
        }),
        "",
    )
}

fn login_pending() -> String {
    "HTTP/1.1 202 Accepted\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".into()
}

fn login_issued(expiration_date: Value) -> String {
    response(
        "200 OK",
        json!({
            "apiKey": ISSUED_KEY,
            "apiKeyName": ISSUED_KEY_NAME,
            "workspaceId": "ws_019f9d57-d0ec-7fa2-9c3e-5b1f0a1d2c3e",
            "workspaceName": "Acme",
            "expirationDate": expiration_date,
        }),
        "",
    )
}

fn request_body(raw: &str) -> Value {
    serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap()
}

/// Checks the start request and every collect request of one login.
fn assert_login_requests(requests: &[String], name: &str) {
    assert!(
        requests[0].starts_with("POST /public/cli/clientAuthentications HTTP/1.1\r\n"),
        "{}",
        requests[0]
    );
    assert_eq!(request_body(&requests[0]), json!({"name": name}));
    for request in &requests[1..] {
        assert!(
            request.starts_with(&format!(
                "POST /public/cli/clientAuthentications/{LOGIN_TOKEN}/apiKey HTTP/1.1\r\n"
            )),
            "{request}"
        );
        assert_eq!(request_body(request), json!({"secret": LOGIN_SECRET}));
    }
    for request in requests {
        assert!(header_values(request, "Authorization").is_empty(), "{request}");
        assert!(!request.lines().next().unwrap().contains(LOGIN_SECRET));
    }
    assert!(!requests[0].contains(LOGIN_SECRET));
}

#[test]
fn login_stores_the_confirmed_key_and_keeps_the_rest_of_the_user_config() {
    let home = Home::new(Some(
        "# LingoHub\nFOO=bar\nexport LINGOHUB_API_KEY=old-key # previous\nLINGOHUB_API_KEY=older-key\n\n# end\n",
    ));
    #[cfg(unix)]
    set_mode(&home.config(), 0o644);
    let stub = Stub::new(vec![
        login_started(900, 1),
        login_pending(),
        login_issued(json!("2027-02-02")),
    ]);
    let out = login_command(
        &stub.origin,
        &home,
        &["login", "--name", " Test Laptop ", "--no-browser", "--json"],
        &[],
    );
    assert_eq!(
        success(&out),
        json!({
            "workspace_id": "ws_019f9d57-d0ec-7fa2-9c3e-5b1f0a1d2c3e",
            "workspace_name": "Acme",
            "api_key_name": ISSUED_KEY_NAME,
            "expiration_date": "2027-02-02",
            "config_file": home.config().to_str().unwrap(),
        })
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Open this link and check that it shows the code WDJB-MJHT"));
    assert!(stderr.contains(VERIFICATION_URL));
    assert!(
        !stderr.contains("Warning"),
        "a stored key overrides nothing: {stderr}"
    );
    let requests = stub.finish();
    assert_eq!(requests.len(), 3);
    assert_login_requests(&requests, "Test Laptop");
    assert!(requests.iter().all(|request| !request.contains("old-key")));
    // The first assignment takes the new key, a later one is dropped, the rest is untouched.
    assert_eq!(
        home.read().unwrap(),
        format!("# LingoHub\nFOO=bar\nexport LINGOHUB_API_KEY={ISSUED_KEY}\n\n# end\n")
    );
    #[cfg(unix)]
    assert_eq!(mode(&home.config()), 0o600);
    assert_eq!(home.entries(), [".lingohub"], "no temporary file is left");
}

#[test]
fn login_prints_a_summary_creates_a_private_config_and_warns_about_overriding_keys() {
    for (args, env, expiration, warning) in [
        (&[][..], &[][..], json!("2027-02-02"), None),
        (
            &[],
            &[("LINGOHUB_API_KEY", "environment-key")],
            Value::Null,
            Some("Warning: LINGOHUB_API_KEY in the environment takes precedence over the stored key."),
        ),
        (
            &["--api-key", "flag-key"],
            &[],
            Value::Null,
            Some("Warning: --api-key takes precedence over the stored key."),
        ),
    ] {
        let home = Home::new(None);
        let stub = Stub::new(vec![login_started(900, 2), login_issued(expiration.clone())]);
        let mut arguments = vec!["login", "--name", "Test Laptop", "--no-browser"];
        arguments.extend_from_slice(args);
        let out = login_command(&stub.origin, &home, &arguments, env);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let validity = if expiration.is_null() {
            " without an expiration date".to_owned()
        } else {
            ", valid until 2027-02-02".to_owned()
        };
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!(
                "Signed in to Acme with the personal API key \"{ISSUED_KEY_NAME}\"{validity}. Stored in {}.\n",
                home.config().display()
            )
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        match warning {
            Some(warning) => assert!(stderr.contains(warning), "{stderr}"),
            None => assert!(!stderr.contains("Warning"), "{stderr}"),
        }
        let requests = stub.finish();
        assert_eq!(requests.len(), 2);
        assert_login_requests(&requests, "Test Laptop");
        assert_eq!(home.read().unwrap(), format!("LINGOHUB_API_KEY={ISSUED_KEY}\n"));
        #[cfg(unix)]
        assert_eq!(mode(&home.config()), 0o600);
    }
}

#[cfg(unix)]
#[test]
fn login_updates_a_symlinked_config_where_it_points() {
    let home = Home::new(None);
    std::fs::create_dir(home.0.join("dotfiles")).unwrap();
    let target = home.0.join("dotfiles").join("lingohub.env");
    std::fs::write(&target, "FOO=bar\n").unwrap();
    std::os::unix::fs::symlink(&target, home.config()).unwrap();
    let stub = Stub::new(vec![login_started(900, 1), login_issued(Value::Null)]);
    let out = login_command(
        &stub.origin,
        &home,
        &["login", "--name", "Test Laptop", "--no-browser"],
        &[],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    stub.finish();
    assert!(
        std::fs::symlink_metadata(home.config())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        format!("FOO=bar\nLINGOHUB_API_KEY={ISSUED_KEY}\n")
    );
    assert_eq!(mode(&target), 0o600);
}

#[test]
fn login_reports_refused_and_failed_confirmations_without_storing_a_key() {
    let config = "FOO=bar\nLINGOHUB_API_KEY=old-key\n";
    let refused = "The login was denied, expired or already used. Run `lingohub login` again.";
    for (reply, expected) in [
        // Denied, unknown or a wrong secret.
        (response("404 Not Found", json!({"status": 404}), ""), refused),
        // Expired or already used.
        (
            response("422 Unprocessable Entity", json!({"status": 422}), ""),
            refused,
        ),
        (
            response(
                "503 Service Unavailable",
                json!({"title": "Service Unavailable", "status": 503}),
                "",
            ),
            "HTTP 503: Service Unavailable",
        ),
    ] {
        let home = Home::new(Some(config));
        let stub = Stub::new(vec![login_started(900, 1), reply]);
        let out = login_command(
            &stub.origin,
            &home,
            &["login", "--name", "Test Laptop", "--no-browser"],
            &[],
        );
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_error_contains(&out, expected);
        assert_eq!(stub.finish().len(), 2, "a final answer is not polled again");
        assert_eq!(home.read().unwrap(), config);
        assert_eq!(home.entries(), [".lingohub"]);
    }
}

#[test]
fn login_expires_after_the_announced_time() {
    let home = Home::new(None);
    let stub = Stub::new(vec![login_started(1, 1), login_pending()]);
    let started = Instant::now();
    let out = login_command(
        &stub.origin,
        &home,
        &["login", "--name", "Test Laptop", "--no-browser", "--json"],
        &[],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert_error_contains(&out, "The login expired. Run `lingohub login` again.");
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(stub.finish().len(), 2);
    assert_eq!(home.read(), None);
}

#[test]
fn login_rejects_failed_starts_and_unsafe_confirmation_links() {
    let mut unsafe_link: Value =
        serde_json::from_str(login_started(900, 1).split_once("\r\n\r\n").unwrap().1).unwrap();
    unsafe_link["verificationUrl"] = json!("http://example.com/?authentication-token=x");
    for (reply, expected) in [
        (
            response("200 OK", unsafe_link, ""),
            "The API returned an invalid login; no login was started",
        ),
        (
            response(
                "400 Bad Request",
                json!({"title": "Bad Request", "status": 400, "detail": "name must not be blank"}),
                "",
            ),
            "HTTP 400: Bad Request\ndetail: name must not be blank",
        ),
    ] {
        let home = Home::new(None);
        let stub = Stub::new(vec![reply]);
        let out = login_command(
            &stub.origin,
            &home,
            &["login", "--name", "Test Laptop", "--no-browser"],
            &[],
        );
        assert_eq!(out.status.code(), Some(1));
        assert_error_contains(&out, expected);
        assert!(!String::from_utf8_lossy(&out.stderr).contains("example.com"));
        assert_eq!(stub.finish().len(), 1);
        assert_eq!(home.read(), None);
    }
}

#[cfg(unix)]
#[test]
fn login_opens_the_link_in_the_browser_and_names_the_device_after_the_host() {
    let home = Home::new(None);
    let bin = home.0.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let marker = home.0.join("opened");
    // macOS opens links with `open`, other Unix systems with `xdg-open`.
    for (name, script) in [
        ("hostname", "#!/bin/sh\necho ' fake-host '\n"),
        (
            "open",
            "#!/bin/sh\nprintf '%s' \"$1\" > \"$LINGOHUB_TEST_MARKER.tmp\"\n/bin/mv \"$LINGOHUB_TEST_MARKER.tmp\" \"$LINGOHUB_TEST_MARKER\"\n",
        ),
    ] {
        std::fs::write(bin.join(name), script).unwrap();
        set_mode(&bin.join(name), 0o755);
    }
    std::fs::copy(bin.join("open"), bin.join("xdg-open")).unwrap();
    let stub = Stub::new(vec![login_started(900, 1), login_issued(Value::Null)]);
    let out = login_command(
        &stub.origin,
        &home,
        &["login", "--json"],
        &[
            ("PATH", bin.to_str().unwrap()),
            ("LINGOHUB_TEST_MARKER", marker.to_str().unwrap()),
        ],
    );
    assert_eq!(success(&out)["workspace_name"], "Acme");
    let requests = stub.finish();
    assert_login_requests(&requests, "fake-host");
    // The browser is started in the background; give it a moment.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), VERIFICATION_URL);
}

#[test]
fn logout_revokes_the_stored_key_and_removes_it() {
    let config = "# LingoHub\nFOO=bar\nexport LINGOHUB_API_KEY=stored-key\n# end\n";
    for (args, env, note) in [
        (
            &["logout"][..],
            &[("LINGOHUB_API_KEY", "environment-key")][..],
            "Note: LINGOHUB_API_KEY in the environment is not affected.",
        ),
        (
            &["--api-key", "flag-key", "logout", "--json"],
            &[],
            "Note: the key passed with --api-key is not affected.",
        ),
    ] {
        let home = Home::new(Some(config));
        #[cfg(unix)]
        set_mode(&home.config(), 0o644);
        let stub = Stub::new(vec![
            "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n".into(),
        ]);
        let out = login_command(&stub.origin, &home, args, env);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        if args.contains(&"--json") {
            assert_eq!(
                serde_json::from_slice::<Value>(&out.stdout).unwrap(),
                json!({
                    "config_file": home.config().to_str().unwrap(),
                    "outcome": "revoked",
                    "revoked": true,
                    "removed": true,
                })
            );
        } else {
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                format!(
                    "Signed out; the key was revoked and removed from {}.\n",
                    home.config().display()
                )
            );
        }
        assert!(String::from_utf8_lossy(&out.stderr).contains(note));
        let requests = stub.finish();
        assert!(
            requests[0].starts_with("DELETE /v2/api-key HTTP/1.1\r\n"),
            "{}",
            requests[0]
        );
        // Only the stored key is revoked, never the one from the environment or the flag.
        assert_eq!(
            header_values(&requests[0], "Authorization"),
            ["Bearer stored-key"]
        );
        assert!(header_values(&requests[0], "If-Match").is_empty());
        assert_eq!(home.read().unwrap(), "# LingoHub\nFOO=bar\n# end\n");
        #[cfg(unix)]
        assert_eq!(mode(&home.config()), 0o600);
        assert_eq!(home.entries(), [".lingohub"]);
    }
}

#[test]
fn logout_removes_invalid_and_workspace_managed_keys_without_revoking_them() {
    let config = "FOO=bar\nLINGOHUB_API_KEY=stored-key\n";
    for (status, outcome, message) in [
        (
            "401 Unauthorized",
            "already_invalid",
            "The stored key was no longer valid and has been removed from {path}.",
        ),
        (
            "403 Forbidden",
            "workspace_managed",
            "The stored key was removed from {path}, but not revoked: it is managed by the workspace, and only a workspace administrator can revoke it.",
        ),
    ] {
        for json_output in [false, true] {
            let home = Home::new(Some(config));
            let code = if status.starts_with("403") {
                "insufficient_permissions"
            } else {
                "unauthorized"
            };
            let stub = Stub::new(vec![response(status, json!({"title": status, "code": code}), "")]);
            let args: &[&str] = if json_output {
                &["logout", "--json"]
            } else {
                &["logout"]
            };
            let out = login_command(&stub.origin, &home, args, &[]);
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            if json_output {
                assert_eq!(
                    serde_json::from_slice::<Value>(&out.stdout).unwrap(),
                    json!({
                        "config_file": home.config().to_str().unwrap(),
                        "outcome": outcome,
                        "revoked": false,
                        "removed": true,
                    })
                );
            } else {
                assert_eq!(
                    String::from_utf8_lossy(&out.stdout),
                    format!(
                        "{}\n",
                        message.replace("{path}", &home.config().display().to_string())
                    )
                );
            }
            let requests = stub.finish();
            assert_eq!(
                header_values(&requests[0], "Authorization"),
                ["Bearer stored-key"]
            );
            assert_eq!(home.read().unwrap(), "FOO=bar\n");
        }
    }
}

#[test]
fn logout_keeps_the_stored_key_when_revocation_fails() {
    let config = "FOO=bar\nLINGOHUB_API_KEY=stored-key\n";
    let home = Home::new(Some(config));
    let stub = Stub::new(vec![response(
        "500 Internal Server Error",
        json!({"title": "Internal Server Error", "status": 500}),
        "",
    )]);
    let out = login_command(&stub.origin, &home, &["logout"], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert_error_contains(&out, "HTTP 500: Internal Server Error");
    stub.finish();
    assert_eq!(home.read().unwrap(), config);

    let out = login_command("http://127.0.0.1:1", &home, &["logout", "--json"], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stderr).unwrap(),
        json!({"error": "Could not connect to the API"})
    );
    assert_eq!(home.read().unwrap(), config);
    assert_eq!(home.entries(), [".lingohub"]);
}

#[test]
fn logout_without_a_stored_key_sends_no_request() {
    let trap = Stub::new(vec![]);
    for config in [
        None,
        Some("FOO=bar\n# LINGOHUB_API_KEY=commented-out\nLINGOHUB_API_KEY_OLD=other\n"),
        Some("LINGOHUB_API_KEY=\n"),
    ] {
        for (args, env) in [
            (&["logout"][..], &[][..]),
            // A key from the environment is never revoked.
            (&["logout"], &[("LINGOHUB_API_KEY", "environment-key")]),
        ] {
            let home = Home::new(config);
            let out = login_command(&trap.origin, &home, args, env);
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                format!("No API key is stored in {}.\n", home.config().display())
            );
            assert_eq!(home.read().as_deref(), config);
        }
    }
    let home = Home::new(None);
    let out = login_command(&trap.origin, &home, &["logout", "--json"], &[]);
    assert_eq!(
        success(&out),
        json!({
            "config_file": home.config().to_str().unwrap(),
            "outcome": "no_stored_key",
            "revoked": false,
            "removed": false,
        })
    );
    trap.finish();
}

#[test]
fn login_and_logout_reject_request_flags_invalid_names_and_remote_http_before_network() {
    let trap = Stub::new(vec![]);
    let home = Home::new(Some("LINGOHUB_API_KEY=stored-key\n"));
    for command in [&["login", "--no-browser"][..], &["logout"]] {
        for (flags, expected) in [
            (
                &["--file", "body.json"][..],
                "This command does not accept --file, --field, --if-match or --idempotency-key",
            ),
            (
                &["--field", "name=\"x\""],
                "This command does not accept --file, --field, --if-match or --idempotency-key",
            ),
            (
                &["--if-match", "\"etag\""],
                "This command does not accept --file, --field, --if-match or --idempotency-key",
            ),
            (
                &["--idempotency-key", "key"],
                "This command does not accept --file, --field, --if-match or --idempotency-key",
            ),
            (
                &["--include-headers"],
                "This command does not expose HTTP response headers",
            ),
        ] {
            let mut args = command.to_vec();
            args.extend_from_slice(flags);
            let out = login_command(&trap.origin, &home, &args, &[]);
            assert_eq!(out.status.code(), Some(2), "{args:?}");
            assert_error_contains(&out, expected);
        }
    }
    let long = "x".repeat(101);
    for name in ["", "   ", long.as_str(), "bad\u{1b}[31mname"] {
        let out = login_command(
            &trap.origin,
            &home,
            &["login", "--no-browser", "--name", name],
            &[],
        );
        assert_eq!(out.status.code(), Some(2), "{name:?}");
        assert_error_contains(
            &out,
            "--name must have 1 to 100 characters and no control characters",
        );
    }
    for command in [
        &["login", "--name", "Test Laptop", "--no-browser"][..],
        &["logout"],
    ] {
        let out = login_command("http://example.com", &home, command, &[]);
        assert_eq!(out.status.code(), Some(2), "{command:?}");
        assert_error_contains(
            &out,
            "Remote API origins require HTTPS; HTTP is allowed only on loopback",
        );
    }
    trap.finish();
    assert_eq!(home.read().unwrap(), "LINGOHUB_API_KEY=stored-key\n");
}
