use super::*;
use oximite_proto::model::MachineCommand;
use oximite_proto::protocol::CommandAck;

#[test]
fn checks_segmented_requests_and_case_insensitive_content_length() {
    let req = b"POST /api/cmd HTTP/1.1\r\nHost: 1.2.3.4\r\ncontent-length: 14\r\n\r\n{\"cmd\":\"stop\"}extra";
    let head_only = &req[..req.len() - 19]; // header without body
    assert_eq!(check_request_complete(head_only, 4096), Ok(None));

    let full = check_request_complete(req, 4096).unwrap().unwrap();
    let parsed = parse_http_request(core::str::from_utf8(&req[..full]).unwrap()).unwrap();
    assert_eq!(
        parsed,
        HttpRoute::PostCmd(ParsedCommand::Machine(MachineCommand::Stop))
    );
}

#[test]
fn enforces_routes_slot_limits_and_sizes() {
    assert_eq!(
        parse_http_request("GET / HTTP/1.1\r\n\r\n"),
        Ok(HttpRoute::Index)
    );
    assert_eq!(
        parse_http_request("GET /index.html HTTP/1.1\r\n\r\n"),
        Ok(HttpRoute::Index)
    );
    assert_eq!(
        parse_http_request("GET /api/telemetry HTTP/1.1\r\n\r\n"),
        Ok(HttpRoute::GetTelemetry)
    );
    assert_eq!(
        parse_http_request("GET /api/settings HTTP/1.1\r\n\r\n"),
        Ok(HttpRoute::GetSettings)
    );
    assert_eq!(
        parse_http_request("GET /api/profiles HTTP/1.1\r\n\r\n"),
        Ok(HttpRoute::GetProfiles)
    );
    assert_eq!(
        parse_http_request("GET /api/profile/9 HTTP/1.1\r\n\r\n"),
        Ok(HttpRoute::GetProfile(9))
    );
    assert_eq!(
        parse_http_request("GET /api/profile/10 HTTP/1.1\r\n\r\n"),
        Err(HttpError::NotFound)
    );
    assert_eq!(
        parse_http_request("GET /unknown HTTP/1.1\r\n\r\n"),
        Err(HttpError::NotFound)
    );

    let big = b"POST /api/cmd HTTP/1.1\r\nContent-Length: 5000\r\n\r\n";
    assert_eq!(check_request_complete(big, 4096), Err(HttpError::TooLarge));

    // Unterminated header reaching max_len must also return TooLarge
    let no_head = [b'A'; 64];
    assert_eq!(
        check_request_complete(&no_head, 64),
        Err(HttpError::TooLarge)
    );
    assert_eq!(check_request_complete(&no_head[..32], 64), Ok(None));
}

#[test]
fn status_lines_error_mapping_and_json_response_encoding() {
    assert_eq!(HttpError::Closed.status_line(), None);
    assert!(HttpError::TooLarge
        .status_line()
        .unwrap()
        .contains("413 Payload Too Large"));
    assert!(HttpError::BadRequest
        .status_line()
        .unwrap()
        .contains("400 Bad Request"));
    assert!(HttpError::NotFound
        .status_line()
        .unwrap()
        .contains("404 Not Found"));
    assert!(HttpError::Busy
        .status_line()
        .unwrap()
        .contains("503 Service Unavailable"));

    assert_eq!(HttpError::from_cmd_err(CmdError::Busy), HttpError::Busy);
    assert_eq!(
        HttpError::from_cmd_err(CmdError::NotFound),
        HttpError::NotFound
    );
    assert_eq!(
        HttpError::from_cmd_err(CmdError::BadJson),
        HttpError::BadRequest
    );
    assert_eq!(
        HttpError::from_cmd_err(CmdError::InvalidSlot),
        HttpError::BadRequest
    );

    let mut buf = [0u8; JSON_RESP_MAX];
    let ack = CommandAck {
        status: "ok",
        ack: 7,
    };
    let len = encode_json_response(&mut buf, &ack).unwrap();
    let s = core::str::from_utf8(&buf[..len]).unwrap();
    assert!(s.starts_with(JSON_OK_HEADER));
    assert!(s.ends_with(r#"{"status":"ok","ack":7}"#));

    // Oversized payload that cannot fit in JSON_RESP_MAX returns None
    let huge =
        ["0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"; 32];
    assert_eq!(encode_json_response(&mut buf, &huge), None);
}
