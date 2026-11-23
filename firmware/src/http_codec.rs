//! Pure HTTP/1.1 request parser, router, and JSON response serializer.

use core::str::from_utf8;
use oximite_proto::model::MAX_PROFILES;
use oximite_proto::protocol::{parse_api_command, CmdError, ParsedCommand};
use serde::Serialize;

pub const JSON_OK_HEADER: &str =
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";

pub const JSON_RESP_MAX: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpError {
    Closed,
    TooLarge,
    BadRequest,
    NotFound,
    Busy,
}

impl HttpError {
    pub fn status_line(&self) -> Option<&'static str> {
        match self {
            HttpError::Closed => None,
            HttpError::TooLarge => {
                Some("HTTP/1.1 413 Payload Too Large\r\nConnection: close\r\n\r\n")
            }
            HttpError::BadRequest => Some("HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n"),
            HttpError::NotFound => Some("HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n"),
            HttpError::Busy => {
                Some("HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\n")
            }
        }
    }

    pub fn from_cmd_err(err: CmdError) -> Self {
        match err {
            CmdError::Busy => HttpError::Busy,
            CmdError::NotFound => HttpError::NotFound,
            CmdError::BadJson
            | CmdError::UnknownCommand
            | CmdError::MissingField
            | CmdError::InvalidSlot => HttpError::BadRequest,
        }
    }
}

#[derive(Serialize)]
pub struct ProfileHeader<'a> {
    pub slot: u8,
    pub name: &'a str,
}

#[derive(Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum HttpRoute {
    Index,
    GetTelemetry,
    GetVersion,
    GetSettings,
    GetProfiles,
    GetProfile(u8),
    PostCmd(ParsedCommand),
}

pub fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

pub fn content_length(head: &str) -> Option<usize> {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
}

/// Checks whether `buf[..n]` contains a full HTTP/1.1 request including its
/// declared `Content-Length` body. Returns `Ok(Some(total_len))` when ready.
pub fn check_request_complete(buf: &[u8], max_len: usize) -> Result<Option<usize>, HttpError> {
    let n = buf.len();
    if let Some(head) = head_end(buf) {
        let head_str = from_utf8(&buf[..head]).map_err(|_| HttpError::BadRequest)?;
        let cl = content_length(head_str).unwrap_or(0);
        let want = head.checked_add(cl).ok_or(HttpError::TooLarge)?;
        if want > max_len {
            return Err(HttpError::TooLarge);
        }
        if n >= want {
            return Ok(Some(want));
        }
    } else if n >= max_len {
        return Err(HttpError::TooLarge);
    }
    Ok(None)
}

pub fn parse_slot(request: &str) -> Result<u8, HttpError> {
    let rest = request
        .strip_prefix("GET /api/profile/")
        .ok_or(HttpError::NotFound)?;
    let end = rest.find(' ').unwrap_or(rest.len());
    let slot = rest[..end].parse::<u8>().map_err(|_| HttpError::NotFound)?;
    if slot >= MAX_PROFILES {
        return Err(HttpError::NotFound);
    }
    Ok(slot)
}

pub fn parse_http_request(request: &str) -> Result<HttpRoute, HttpError> {
    if request.starts_with("GET / ") || request.starts_with("GET /index.html ") {
        Ok(HttpRoute::Index)
    } else if request.starts_with("GET /api/telemetry ") {
        Ok(HttpRoute::GetTelemetry)
    } else if request.starts_with("GET /api/version ") {
        Ok(HttpRoute::GetVersion)
    } else if request.starts_with("GET /api/settings ") {
        Ok(HttpRoute::GetSettings)
    } else if request.starts_with("GET /api/profiles ") {
        Ok(HttpRoute::GetProfiles)
    } else if request.starts_with("GET /api/profile/") {
        Ok(HttpRoute::GetProfile(parse_slot(request)?))
    } else if request.starts_with("POST /api/cmd ") {
        let (_, body) = request
            .split_once("\r\n\r\n")
            .ok_or(HttpError::BadRequest)?;
        let cmd = parse_api_command(body).map_err(HttpError::from_cmd_err)?;
        Ok(HttpRoute::PostCmd(cmd))
    } else {
        Err(HttpError::NotFound)
    }
}

pub fn encode_json_response<T: Serialize>(
    buf: &mut [u8; JSON_RESP_MAX],
    value: &T,
) -> Option<usize> {
    let hdr = JSON_OK_HEADER.as_bytes();
    buf[..hdr.len()].copy_from_slice(hdr);
    let len = serde_json_core::to_slice(value, &mut buf[hdr.len()..]).ok()?;
    Some(hdr.len() + len)
}

#[cfg(test)]
#[path = "../../host-tests/src/tests/http_codec.rs"]
mod tests;
