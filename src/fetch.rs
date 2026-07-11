//! Trusted downloads into a run's temporary scratch space. The box supplies a
//! URL and relative destination; the host derives identity, applies normal
//! egress policy and budgets, streams with a hard bound, hashes exact bytes,
//! and journals a durable receipt.

use crate::util::{now_rfc3339, root};
use reqwest::header::{HeaderName, HeaderValue, LOCATION};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const MAX_REDIRECTS: usize = 5;

#[derive(Debug, Serialize)]
pub struct FetchReceipt {
    pub id: String,
    pub source: String,
    pub final_url: String,
    pub retrieved_at: String,
    pub media_type: String,
    pub bytes: u64,
    pub sha256: String,
    pub scratch: ScratchPointer,
}

#[derive(Debug, Serialize)]
pub struct ScratchPointer {
    pub path: String,
    pub durability: &'static str,
}

struct Downloaded {
    final_url: String,
    media_type: String,
    bytes: u64,
    sha256: String,
}

pub async fn execute(worker: &str, run_id: &str, payload: &Value) -> Result<Value, String> {
    let source = payload
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or("fetch-to-scratch needs a URL")?;
    let relative_text = payload
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or("fetch-to-scratch needs a relative scratch path")?;
    let source_url =
        reqwest::Url::parse(source).map_err(|error| format!("invalid URL: {error}"))?;
    let relative = safe_relative_path(relative_text)?;
    let short_worker = worker.strip_prefix("org/").unwrap_or(worker);
    let record = crate::runlog::load(run_id).ok_or("download has no active run record")?;
    if record.worker != short_worker || record.state != "running" {
        return Err("download run identity is stale or does not match the worker".into());
    }
    let scratch = root().join("runs").join(run_id).join("scratch");
    if !scratch.is_dir() {
        return Err("download scratch directory is not active".into());
    }
    let policy = crate::storage::load(short_worker).scratch;
    let (used_bytes, used_files) = scratch_usage(&scratch)?;
    if used_files >= policy.max_files {
        return Err(format!(
            "scratch already has the maximum {} files",
            policy.max_files
        ));
    }
    let available = policy
        .max_bytes
        .checked_sub(used_bytes)
        .ok_or("scratch is already over its byte limit")?;
    if available == 0 {
        return Err("scratch has no byte capacity remaining".into());
    }

    let fetch_id = format!("fetch_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    crate::journal::append_required(
        worker,
        run_id,
        "fetch-requested",
        json!({
            "id": fetch_id,
            "source": source_url.as_str(),
            "scratch": { "path": relative_text, "durability": "transient" },
        }),
    )?;

    let result = download(
        worker,
        run_id,
        &source_url,
        &scratch,
        &relative,
        available,
        &fetch_id,
    )
    .await;
    let downloaded = match result {
        Ok(value) => value,
        Err(error) => {
            let journal = crate::journal::append_required(
                worker,
                run_id,
                "fetch-failed",
                json!({ "id": fetch_id, "source": source_url.as_str(), "error": error }),
            );
            return match journal {
                Ok(()) => Err(error),
                Err(journal_error) => Err(format!(
                    "{error}; could not record fetch failure: {journal_error}"
                )),
            };
        }
    };
    let receipt = FetchReceipt {
        id: fetch_id.clone(),
        source: source_url.to_string(),
        final_url: downloaded.final_url,
        retrieved_at: now_rfc3339(),
        media_type: downloaded.media_type,
        bytes: downloaded.bytes,
        sha256: downloaded.sha256,
        scratch: ScratchPointer {
            path: relative_text.into(),
            durability: "transient",
        },
    };
    crate::journal::append_required(
        worker,
        run_id,
        "fetch-completed",
        serde_json::to_value(&receipt).map_err(|error| error.to_string())?,
    )?;
    let _ = crate::runlog::record_fetch_receipt(run_id, &fetch_id);
    serde_json::to_value(receipt).map_err(|error| error.to_string())
}

async fn download(
    subject: &str,
    run_id: &str,
    source: &reqwest::Url,
    scratch: &Path,
    relative: &Path,
    max_bytes: u64,
    fetch_id: &str,
) -> Result<Downloaded, String> {
    let staging = root().join("runs").join(run_id).join("fetch-staging");
    std::fs::create_dir_all(&staging).map_err(|error| error.to_string())?;
    let temporary = staging.join(format!("{fetch_id}.part"));
    let result = stream_download(subject, source, &temporary, max_bytes).await;
    let downloaded = match result {
        Ok(value) => value,
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
    };

    let parent = prepare_parent(scratch, relative)?;
    let name = relative
        .file_name()
        .ok_or("scratch destination needs a filename")?;
    let destination = parent.join(name);
    if let Err(error) = std::fs::hard_link(&temporary, &destination) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!(
            "could not create scratch file {}: {error}",
            relative.display()
        ));
    }
    std::fs::remove_file(&temporary).map_err(|error| error.to_string())?;
    let _ = std::fs::remove_dir(&staging);
    Ok(downloaded)
}

async fn stream_download(
    subject: &str,
    source: &reqwest::Url,
    temporary: &Path,
    max_bytes: u64,
) -> Result<Downloaded, String> {
    let mut current = source.clone();
    for redirect in 0..=MAX_REDIRECTS {
        let client = public_client(&current).await?;
        let injected = crate::proxy::authorize_download(subject, &current).await?;
        let mut request = client
            .get(current.clone())
            .header("user-agent", "Roster/0.0.1")
            .header("accept", "*/*");
        for (name, value) in injected {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| format!("policy produced invalid header name {name}"))?;
            let value = HeaderValue::from_str(&value)
                .map_err(|_| "policy produced an invalid header value".to_string())?;
            request = request.header(name, value);
        }
        let mut response = request.send().await.map_err(|error| error.to_string())?;
        if response.status().is_redirection() {
            if redirect == MAX_REDIRECTS {
                return Err(format!("download exceeded {MAX_REDIRECTS} redirects"));
            }
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or("download redirect has no valid Location header")?;
            current = current
                .join(location)
                .map_err(|error| format!("invalid redirect URL: {error}"))?;
            continue;
        }
        if !response.status().is_success() {
            return Err(format!(
                "download returned HTTP {} from {}",
                response.status(),
                current
            ));
        }
        if let Some(length) = response.content_length() {
            if length > max_bytes {
                return Err(format!(
                    "download declares {length} bytes, over the {max_bytes} byte remaining scratch limit"
                ));
            }
        }
        let media_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(temporary)
            .await
            .map_err(|error| error.to_string())?;
        let mut hasher = Sha256::new();
        let mut bytes = 0u64;
        while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
            account_chunk(&mut hasher, &mut bytes, &chunk, max_bytes)?;
            file.write_all(&chunk)
                .await
                .map_err(|error| error.to_string())?;
        }
        file.sync_all().await.map_err(|error| error.to_string())?;
        return Ok(Downloaded {
            final_url: current.to_string(),
            media_type,
            bytes,
            sha256: format!("{:x}", hasher.finalize()),
        });
    }
    Err("download redirect loop ended unexpectedly".into())
}

async fn public_client(url: &reqwest::Url) -> Result<reqwest::Client, String> {
    let host = url.host_str().ok_or("download URL has no host")?;
    let port = url
        .port_or_known_default()
        .ok_or("download URL has no known port")?;
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| format!("could not resolve download host {host}: {error}"))?
        .collect();
    let address = addresses
        .into_iter()
        .find(|address| is_public_ip(address.ip()))
        .ok_or_else(|| format!("download host {host} has no public network address"))?;
    reqwest::Client::builder()
        .no_proxy()
        .resolve(host, address)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|error| error.to_string())
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || octets[0] == 0
        || octets[0] >= 224
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 198 && matches!(octets[1], 18 | 19)))
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(ipv4) = ip.to_ipv4() {
        return is_public_v4(ipv4);
    }
    let segments = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0)
}

fn account_chunk(
    hasher: &mut Sha256,
    bytes: &mut u64,
    chunk: &[u8],
    max_bytes: u64,
) -> Result<(), String> {
    let next = bytes
        .checked_add(chunk.len() as u64)
        .ok_or("download size overflow")?;
    if next > max_bytes {
        return Err(format!(
            "download exceeded the {max_bytes} byte remaining scratch limit"
        ));
    }
    *bytes = next;
    hasher.update(chunk);
    Ok(())
}

fn safe_relative_path(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if path.is_absolute() {
        return Err("scratch path must be relative".into());
    }
    let mut count = 0usize;
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err("scratch path cannot contain . or ..".into());
        };
        let name = name.to_str().ok_or("scratch path must be UTF-8")?;
        if name.is_empty() || name.starts_with('.') {
            return Err("scratch path cannot contain hidden components".into());
        }
        count += 1;
    }
    if count == 0 || path.file_name().is_none() {
        return Err("scratch path needs a filename".into());
    }
    Ok(path.to_path_buf())
}

fn prepare_parent(scratch: &Path, relative: &Path) -> Result<PathBuf, String> {
    let scratch = std::fs::canonicalize(scratch).map_err(|error| error.to_string())?;
    let mut parent = scratch.clone();
    if let Some(relative_parent) = relative.parent() {
        for component in relative_parent.components() {
            let Component::Normal(name) = component else {
                return Err("unsafe scratch parent".into());
            };
            parent.push(name);
            match std::fs::symlink_metadata(&parent) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err("scratch destination cannot traverse a symlink".into())
                }
                Ok(metadata) if !metadata.is_dir() => {
                    return Err("scratch destination parent is not a directory".into())
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&parent).map_err(|error| error.to_string())?;
                }
                Err(error) => return Err(error.to_string()),
            }
        }
    }
    let resolved = std::fs::canonicalize(&parent).map_err(|error| error.to_string())?;
    if !resolved.starts_with(&scratch) {
        return Err("scratch destination escapes the run".into());
    }
    Ok(resolved)
}

fn scratch_usage(root: &Path) -> Result<(u64, usize), String> {
    fn walk(path: &Path, bytes: &mut u64, files: &mut usize) -> Result<(), String> {
        for entry in std::fs::read_dir(path).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let metadata =
                std::fs::symlink_metadata(entry.path()).map_err(|error| error.to_string())?;
            if metadata.file_type().is_symlink() {
                return Err("scratch contains a symlink; governed download refused".into());
            }
            if metadata.is_dir() {
                walk(&entry.path(), bytes, files)?;
            } else if metadata.is_file() {
                *bytes = bytes
                    .checked_add(metadata.len())
                    .ok_or("scratch size overflow")?;
                *files = files.checked_add(1).ok_or("scratch file count overflow")?;
            } else {
                return Err("scratch contains an unsupported file type".into());
            }
        }
        Ok(())
    }
    let mut bytes = 0;
    let mut files = 0;
    walk(root, &mut bytes, &mut files)?;
    Ok((bytes, files))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_paths_are_relative_and_bounded() {
        assert_eq!(
            safe_relative_path("downloads/report.pdf").unwrap(),
            PathBuf::from("downloads/report.pdf")
        );
        assert!(safe_relative_path("../report.pdf").is_err());
        assert!(safe_relative_path("/tmp/report.pdf").is_err());
        assert!(safe_relative_path("downloads/.hidden").is_err());
    }

    #[test]
    fn scratch_usage_rejects_symlinks() {
        let dir = std::env::temp_dir().join(format!("roster-fetch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("one"), b"1234").unwrap();
        assert_eq!(scratch_usage(&dir).unwrap(), (4, 1));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("one", dir.join("link")).unwrap();
            assert!(scratch_usage(&dir).is_err());
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn chunk_accounting_hashes_exact_bounded_bytes() {
        let mut hasher = Sha256::new();
        let mut bytes = 0;
        account_chunk(&mut hasher, &mut bytes, b"a", 3).unwrap();
        account_chunk(&mut hasher, &mut bytes, b"bc", 3).unwrap();
        assert_eq!(bytes, 3);
        assert_eq!(
            format!("{:x}", hasher.finalize()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let mut hasher = Sha256::new();
        let mut bytes = 0;
        assert!(account_chunk(&mut hasher, &mut bytes, b"abcd", 3).is_err());
        assert_eq!(bytes, 0);
    }

    #[test]
    fn downloads_reject_non_public_networks() {
        assert!(!is_public_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("10.1.2.3".parse().unwrap()));
        assert!(!is_public_ip("169.254.169.254".parse().unwrap()));
        assert!(!is_public_ip("::1".parse().unwrap()));
        assert!(!is_public_ip("fd00::1".parse().unwrap()));
        assert!(is_public_ip("1.1.1.1".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
    }
}
