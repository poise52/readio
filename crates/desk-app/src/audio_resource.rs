use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::http::{Method, Request, Response, StatusCode, header};
use uuid::Uuid;

const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_LEASES: usize = 64;
const LEASE_TTL: Duration = Duration::from_hours(24);

#[derive(Debug)]
struct Lease {
    data: Arc<Vec<u8>>,
    mime: String,
    touched: Instant,
}
#[derive(Debug, Default)]
pub struct AudioResources {
    leases: Mutex<HashMap<String, Lease>>,
}
impl AudioResources {
    pub fn insert(&self, data: Vec<u8>, mime: String) -> Result<String, &'static str> {
        if data.is_empty()
            || data.len() > 32 * 1024 * 1024
            || !matches!(
                mime.as_str(),
                "audio/mpeg" | "audio/wav" | "audio/x-wav" | "audio/ogg" | "audio/mp4"
            )
        {
            return Err("Некорректный аудиоресурс");
        }
        let mut leases = self
            .leases
            .lock()
            .map_err(|_| "Реестр аудиоресурсов недоступен")?;
        leases.retain(|_, lease| lease.touched.elapsed() < LEASE_TTL);
        if leases.len() >= MAX_LEASES
            || leases
                .values()
                .map(|lease| lease.data.len())
                .sum::<usize>()
                .saturating_add(data.len())
                > MAX_BYTES
        {
            return Err("Превышен лимит аудиоресурсов");
        }
        let token = Uuid::new_v4().simple().to_string();
        leases.insert(
            token.clone(),
            Lease {
                data: Arc::new(data),
                mime,
                touched: Instant::now(),
            },
        );
        drop(leases);
        Ok(token)
    }
    pub fn clear(&self) {
        if let Ok(mut leases) = self.leases.lock() {
            leases.clear();
        }
    }
    pub fn release(&self, token: &str) {
        if let Ok(mut leases) = self.leases.lock() {
            leases.remove(token);
        }
    }
    pub fn respond(&self, request: &Request<Vec<u8>>) -> Response<Vec<u8>> {
        let empty = |status| {
            let mut response = Response::new(Vec::new());
            *response.status_mut() = status;
            response
        };
        if request.method() != Method::GET && request.method() != Method::HEAD {
            return empty(StatusCode::METHOD_NOT_ALLOWED);
        }
        let token = request.uri().path().trim_start_matches('/');
        if token.len() != 32
            || !token.bytes().all(|b| b.is_ascii_hexdigit())
            || request.uri().query().is_some()
        {
            return empty(StatusCode::NOT_FOUND);
        }
        let Ok(mut leases) = self.leases.lock() else {
            return empty(StatusCode::INTERNAL_SERVER_ERROR);
        };
        let Some(lease) = leases.get_mut(token) else {
            return empty(StatusCode::NOT_FOUND);
        };
        if lease.touched.elapsed() >= LEASE_TTL {
            leases.remove(token);
            return empty(StatusCode::NOT_FOUND);
        }
        lease.touched = Instant::now();
        let len = lease.data.len();
        let mut start = 0;
        let mut end = len - 1;
        let range = request.headers().get(header::RANGE);
        if let Some(range) = range {
            let parsed = range
                .to_str()
                .ok()
                .and_then(|s| s.strip_prefix("bytes="))
                .and_then(|s| s.split_once('-'))
                .and_then(|(first, last)| {
                    if first.is_empty() {
                        let suffix: usize = last.parse().ok()?;
                        if suffix == 0 {
                            return None;
                        }
                        Some((len.saturating_sub(suffix), len - 1))
                    } else {
                        let from: usize = first.parse().ok()?;
                        let to = if last.is_empty() {
                            len - 1
                        } else {
                            last.parse::<usize>().ok()?.min(len - 1)
                        };
                        (from < len && from <= to).then_some((from, to))
                    }
                });
            let Some((from, to)) = parsed else {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                    .body(Vec::new())
                    .unwrap_or_else(|_| empty(StatusCode::INTERNAL_SERVER_ERROR));
            };
            start = from;
            end = to;
        }
        let mut builder = Response::builder()
            .status(if range.is_some() {
                StatusCode::PARTIAL_CONTENT
            } else {
                StatusCode::OK
            })
            .header(header::CONTENT_TYPE, &lease.mime)
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, end - start + 1)
            .header(header::CACHE_CONTROL, "no-store");
        if range.is_some() {
            builder = builder.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"));
        }
        builder
            .body(if request.method() == Method::HEAD {
                Vec::new()
            } else {
                lease.data[start..=end].to_vec()
            })
            .unwrap_or_else(|_| empty(StatusCode::INTERNAL_SERVER_ERROR))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn lease_serves_ranges_and_release_revokes_access() {
        let resources = AudioResources::default();
        let token = resources
            .insert(b"abcdef".to_vec(), "audio/mpeg".into())
            .unwrap();
        let request = Request::builder()
            .uri(format!("/{token}"))
            .header(header::RANGE, "bytes=2-4")
            .body(Vec::new())
            .unwrap();
        let response = resources.respond(&request);
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-4/6");
        assert_eq!(response.body(), b"cde");
        let invalid = Request::builder()
            .uri(format!("/{token}"))
            .header(header::RANGE, "bytes=9-")
            .body(Vec::new())
            .unwrap();
        assert_eq!(
            resources.respond(&invalid).status(),
            StatusCode::RANGE_NOT_SATISFIABLE
        );
        resources.release(&token);
        assert_eq!(resources.respond(&request).status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn capacity_never_evicts_live_leases() {
        let resources = AudioResources::default();
        let first = resources.insert(vec![1], "audio/mpeg".into()).unwrap();
        for _ in 1..MAX_LEASES {
            resources.insert(vec![2], "audio/mpeg".into()).unwrap();
        }
        assert!(resources.insert(vec![3], "audio/mpeg".into()).is_err());
        let request = Request::builder()
            .uri(format!("/{first}"))
            .body(Vec::new())
            .unwrap();
        assert_eq!(resources.respond(&request).body(), &[1]);
        resources.release(&first);
        assert!(resources.insert(vec![3], "audio/mpeg".into()).is_ok());
    }
    #[test]
    fn expiry_head_queries_and_byte_capacity() {
        let resources = AudioResources::default();
        let first = resources
            .insert(vec![7; 32 * 1024 * 1024], "audio/mpeg".into())
            .unwrap();
        let second = resources
            .insert(vec![8; 32 * 1024 * 1024], "audio/mpeg".into())
            .unwrap();
        assert!(resources.insert(vec![1], "audio/mpeg".into()).is_err());
        let head = Request::builder()
            .method(Method::HEAD)
            .uri(format!("/{first}"))
            .body(Vec::new())
            .unwrap();
        let response = resources.respond(&head);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH],
            (32 * 1024 * 1024).to_string()
        );
        assert_eq!(response.body().len(), 0);
        let query = Request::builder()
            .uri(format!("/{first}?x=1"))
            .body(Vec::new())
            .unwrap();
        assert_eq!(resources.respond(&query).status(), StatusCode::NOT_FOUND);
        resources
            .leases
            .lock()
            .unwrap()
            .get_mut(&second)
            .unwrap()
            .touched = Instant::now().checked_sub(LEASE_TTL).unwrap();
        assert!(resources.insert(vec![1], "audio/mpeg".into()).is_ok());
        let expired = Request::builder()
            .uri(format!("/{second}"))
            .body(Vec::new())
            .unwrap();
        assert_eq!(resources.respond(&expired).status(), StatusCode::NOT_FOUND);
        resources.clear();
        assert_eq!(resources.respond(&head).status(), StatusCode::NOT_FOUND);
    }
}
