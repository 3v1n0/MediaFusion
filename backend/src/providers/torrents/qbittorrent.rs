/// qBittorrent + WebDAV torrent streaming provider.
///
/// Adds magnet/torrent to qBittorrent, waits for download progress, then serves
/// the selected file via credentialed WebDAV URL.
use std::time::{Duration, Instant};

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use reqwest::{
    Client,
    header::{COOKIE, HeaderMap, SET_COOKIE},
};
use serde_json::Value;

use crate::providers::{
    ProviderError,
    file_selection::{FileEntry, select_torrent_file_index},
    usenet::webdav,
};

/// Keeps the previous 60 x 5s behaviour for profiles that never set one.
const DEFAULT_DOWNLOAD_WAIT_TIMEOUT_SECS: i64 = 300;

#[derive(Debug, Clone)]
struct QbConfig {
    qb_url: String,
    qb_user: String,
    qb_pass: String,
    webdav_url: String,
    webdav_user: String,
    webdav_pass: String,
    downloads_paths: Vec<String>,
    play_video_after: i32,
    /// How long to wait for a torrent to reach `play_video_after`, in seconds.
    download_wait_timeout_secs: u64,
    seeding_time_limit: i32,
    seeding_ratio_limit: f64,
    category: String,
}

fn parse_config(raw: &Value) -> Result<QbConfig, ProviderError> {
    let str_field = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|k| raw.get(*k).and_then(|v| v.as_str()))
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    };

    let qb_url = str_field(&["qbittorrent_url", "qur", "url"]).ok_or_else(|| {
        ProviderError::api(
            "qBittorrent: no qbittorrent_url in config",
            "invalid_config.mp4",
        )
    })?;
    let qb_user = str_field(&["qbittorrent_username", "qus"]).unwrap_or_default();
    let qb_pass = str_field(&["qbittorrent_password", "qpw"]).unwrap_or_default();
    let webdav_url = str_field(&["webdav_url", "wur"]).ok_or_else(|| {
        ProviderError::api("qBittorrent: no webdav_url in config", "invalid_config.mp4")
    })?;
    let webdav_user = str_field(&["webdav_username", "wus"]).unwrap_or_default();
    let webdav_pass = str_field(&["webdav_password", "wpw"]).unwrap_or_default();

    let primary = str_field(&["webdav_downloads_path", "wdp"]).unwrap_or_else(|| "/".to_string());
    let mut downloads_paths = vec![primary];
    if let Some(extra) = raw.get("webdav_extra_paths").or_else(|| raw.get("wep"))
        && let Some(arr) = extra.as_array()
    {
        for v in arr {
            if let Some(s) = v.as_str() {
                let t = s.trim();
                if !t.is_empty() && !downloads_paths.iter().any(|p| p == t) {
                    downloads_paths.push(t.to_string());
                }
            }
        }
    }

    let play_video_after = raw
        .get("play_video_after")
        .or_else(|| raw.get("pva"))
        .and_then(|v| v.as_i64())
        .unwrap_or(100)
        .clamp(0, 100) as i32;
    let download_wait_timeout_secs = raw
        .get("download_wait_timeout")
        .or_else(|| raw.get("dwt"))
        .and_then(|v| v.as_i64())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_DOWNLOAD_WAIT_TIMEOUT_SECS) as u64;
    let seeding_time_limit = raw
        .get("seeding_time_limit")
        .or_else(|| raw.get("stl"))
        .and_then(|v| v.as_i64())
        .unwrap_or(1440) as i32;
    let seeding_ratio_limit = raw
        .get("seeding_ratio_limit")
        .or_else(|| raw.get("srl"))
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0);
    let category = str_field(&["category", "cat"]).unwrap_or_else(|| "MediaFusion".to_string());

    Ok(QbConfig {
        qb_url: qb_url.trim_end_matches('/').to_string(),
        qb_user,
        qb_pass,
        webdav_url,
        webdav_user,
        webdav_pass,
        downloads_paths,
        play_video_after,
        download_wait_timeout_secs,
        seeding_time_limit,
        seeding_ratio_limit,
        category,
    })
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| {
            value
                .split(';')
                .next()
                .filter(|cookie| cookie.starts_with("SID="))
                .map(str::to_owned)
        })
}

async fn qb_login(http: &Client, cfg: &QbConfig) -> Result<String, ProviderError> {
    let url = format!("{}/api/v2/auth/login", cfg.qb_url);
    let resp = http
        .post(&url)
        .form(&[
            ("username", cfg.qb_user.as_str()),
            ("password", cfg.qb_pass.as_str()),
        ])
        .send()
        .await?;
    let status = resp.status();
    let cookie = session_cookie(resp.headers());
    let text = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::FORBIDDEN || text.to_lowercase().contains("fail") {
        return Err(ProviderError::api(
            "Invalid qBittorrent credentials",
            "invalid_credentials.mp4",
        ));
    }
    if !status.is_success() {
        return Err(ProviderError::api(
            format!("qBittorrent login failed (HTTP {status})"),
            "qbittorrent_error.mp4",
        ));
    }
    cookie.ok_or_else(|| {
        ProviderError::api(
            "qBittorrent login did not return a session cookie",
            "qbittorrent_error.mp4",
        )
    })
}

/// One file as qBittorrent knows it: `name` is relative to the torrent's save
/// path and therefore carries the full in-torrent path.
#[derive(Debug, Clone, PartialEq)]
struct QbFile {
    index: usize,
    name: String,
    size: i64,
}

/// Per-file metadata straight from qBittorrent.
///
/// Without this the WebDAV walk is the only source, and it cannot report a
/// size or a stable file index, so selection degenerates: the "largest video"
/// tie-break compares zeros and two files sharing a basename are
/// indistinguishable.
async fn qb_torrent_files(
    http: &Client,
    cfg: &QbConfig,
    session_cookie: &str,
    info_hash: &str,
) -> Result<Vec<QbFile>, ProviderError> {
    let url = format!("{}/api/v2/torrents/files?hash={info_hash}", cfg.qb_url);
    let arr: Vec<Value> = http
        .get(&url)
        .header(COOKIE, session_cookie)
        .send()
        .await?
        .json()
        .await?;
    Ok(arr
        .iter()
        .filter_map(|v| {
            let name = v.get("name").and_then(|n| n.as_str())?;
            // `index` only exists since API v2.8.2; fall back to array order.
            let index = v
                .get("index")
                .and_then(|i| i.as_i64())
                .map_or_else(|| usize::MAX, |i| i.max(0) as usize);
            Some(QbFile {
                index,
                name: name.replace('\\', "/"),
                size: v.get("size").and_then(|s| s.as_i64()).unwrap_or(0),
            })
        })
        .collect())
}

async fn qb_torrent_info(
    http: &Client,
    cfg: &QbConfig,
    session_cookie: &str,
    info_hash: &str,
) -> Result<Option<f64>, ProviderError> {
    let url = format!("{}/api/v2/torrents/info?hashes={info_hash}", cfg.qb_url);
    let arr: Vec<Value> = http
        .get(&url)
        .header(COOKIE, session_cookie)
        .send()
        .await?
        .json()
        .await?;
    Ok(arr
        .first()
        .and_then(|t| t.get("progress").and_then(|v| v.as_f64())))
}

async fn qb_add_torrent(
    http: &Client,
    cfg: &QbConfig,
    session_cookie: &str,
    magnet: &str,
    info_hash: &str,
    torrent_name: &str,
    torrent_file: Option<&[u8]>,
    is_private: bool,
) -> Result<(), ProviderError> {
    if is_private && torrent_file.is_some() {
        qb_set_preferences(http, cfg, session_cookie, true).await?;
    }

    let url = format!("{}/api/v2/torrents/add", cfg.qb_url);
    let resp = if let Some(bytes) = torrent_file.filter(|b| !b.is_empty()) {
        let part = reqwest::multipart::Part::bytes(bytes.to_vec())
            .file_name(format!("{torrent_name}.torrent"))
            .mime_str("application/x-bittorrent")
            .map_err(|e| {
                ProviderError::api(format!("torrent multipart: {e}"), "add_torrent_failed.mp4")
            })?;
        http.post(&url)
            .header(COOKIE, session_cookie)
            .multipart(
                reqwest::multipart::Form::new()
                    .part("torrents", part)
                    .text("savepath", info_hash.to_string())
                    .text("sequentialDownload", "true")
                    .text("category", cfg.category.clone())
                    .text("seedingTimeLimit", cfg.seeding_time_limit.to_string())
                    .text("ratioLimit", cfg.seeding_ratio_limit.to_string()),
            )
            .send()
            .await?
    } else {
        http.post(&url)
            .header(COOKIE, session_cookie)
            .form(&[
                ("urls", magnet),
                ("savepath", info_hash),
                ("sequentialDownload", "true"),
                ("category", &cfg.category),
                ("seedingTimeLimit", &cfg.seeding_time_limit.to_string()),
                ("ratioLimit", &cfg.seeding_ratio_limit.to_string()),
            ])
            .send()
            .await?
    };

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default().to_lowercase();
    if status.is_success() || is_duplicate_torrent_error(&text) {
        return Ok(());
    }
    Err(ProviderError::api(
        format!("Failed to add torrent to qBittorrent: {text}"),
        "add_torrent_failed.mp4",
    ))
}

fn is_duplicate_torrent_error(text: &str) -> bool {
    [
        "already in the list",
        "already in the download list",
        "torrent is already present",
        "already present",
        "duplicate torrent",
        "is already queued",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
}

async fn qb_set_preferences(
    http: &Client,
    cfg: &QbConfig,
    session_cookie: &str,
    disable_dht: bool,
) -> Result<(), ProviderError> {
    if !disable_dht {
        return Ok(());
    }
    let url = format!("{}/api/v2/app/setPreferences", cfg.qb_url);
    let json = serde_json::json!({"dht": false, "pex": false, "lsd": false}).to_string();
    let resp = http
        .post(&url)
        .header(COOKIE, session_cookie)
        .form(&[("json", json.as_str())])
        .send()
        .await?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(ProviderError::api(
            "Failed to set qBittorrent preferences",
            "add_torrent_failed.mp4",
        ))
    }
}

async fn qb_add_magnet(
    http: &Client,
    cfg: &QbConfig,
    session_cookie: &str,
    magnet: &str,
    info_hash: &str,
) -> Result<(), ProviderError> {
    qb_add_torrent(
        http,
        cfg,
        session_cookie,
        magnet,
        info_hash,
        info_hash,
        None,
        false,
    )
    .await
}

async fn wait_for_progress(
    http: &Client,
    cfg: &QbConfig,
    session_cookie: &str,
    info_hash: &str,
) -> Result<(), ProviderError> {
    let threshold = cfg.play_video_after as f64 / 100.0;
    let deadline = Instant::now() + Duration::from_secs(cfg.download_wait_timeout_secs);
    let mut last_progress: Option<f64> = None;

    loop {
        if let Some(progress) = qb_torrent_info(http, cfg, session_cookie, info_hash).await? {
            last_progress = Some(progress);
            if progress >= threshold {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            let pct = last_progress.map_or(0.0, |p| p * 100.0);
            tracing::debug!(
                info_hash = %info_hash,
                progress_pct = %pct,
                target_pct = cfg.play_video_after,
                waited_secs = cfg.download_wait_timeout_secs,
                "qBittorrent download wait timed out"
            );
            return Err(ProviderError::api(
                format!(
                    "Torrent at {pct:.1}% after {}s, still below the configured {}% — retry once it downloads further",
                    cfg.download_wait_timeout_secs, cfg.play_video_after
                ),
                "torrent_not_downloaded.mp4",
            ));
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn list_webdav_files_recursive(
    http: &Client,
    cfg: &QbConfig,
    root: &str,
) -> Result<Vec<FileEntry>, ProviderError> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_string()];
    let mut idx = 0usize;

    while let Some(dir) = stack.pop() {
        let hrefs = webdav::list(
            http,
            &cfg.webdav_url,
            dir.trim_start_matches('/'),
            &cfg.webdav_user,
            &cfg.webdav_pass,
        )
        .await?;

        for href in hrefs {
            if let Some(name) = classify_href(&href, &dir) {
                match name {
                    Href::Dir(name) => stack.push(format!("{dir}/{name}")),
                    Href::File(name)
                        if super::super::usenet::is_video_name(&name.to_lowercase()) =>
                    {
                        files.push(FileEntry {
                            index: idx,
                            name: href.clone(),
                            size: 0,
                        });
                        idx += 1;
                    }
                    Href::File(_) => {}
                }
            }
        }
    }
    Ok(files)
}

enum Href<'a> {
    Dir(&'a str),
    File(&'a str),
}

/// Classify a PROPFIND href relative to the directory that was listed.
///
/// A collection href carries a trailing `/`, so its basename has to be taken
/// after stripping it — `rsplit('/').next()` on `.../Some.Release/` yields an
/// empty string, which silently dropped every subdirectory and made the walk
/// stop at the top. Multi-file torrents nest their video in such a directory,
/// so they never resolved at any download progress.
fn classify_href<'a>(href: &'a str, dir: &str) -> Option<Href<'a>> {
    let is_dir = href.ends_with('/');
    let trimmed = href.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
    if name.is_empty() {
        return None;
    }
    // PROPFIND echoes the collection itself as the first href; descending into
    // it would request a path that does not exist. `dir` is what we asked for
    // (no leading slash) while hrefs are absolute, so normalise both sides.
    if trimmed.trim_matches('/') == dir.trim_matches('/') {
        return None;
    }
    Some(if is_dir {
        Href::Dir(name)
    } else {
        Href::File(name)
    })
}

/// Characters that must not appear literally in a URL path segment. The
/// sub-delims and `:`/`@` are legal there, so unlike the userinfo set only the
/// genuinely structural ones are escaped.
const PATH_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'[')
    .add(b']');

/// Percent-encode each `/`-separated segment of a server-side file path.
///
/// qBittorrent reports names as they sit on disk (`Some.Release/Ben The Men.mkv`)
/// while a WebDAV href is percent-encoded, so the path has to be encoded before
/// it can go into a streaming URL.
fn encode_path_segments(path: &str) -> String {
    path.split('/')
        .map(|seg| utf8_percent_encode(seg, PATH_SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// Resolve the WebDAV href of one of a torrent's files.
///
/// `wdp` is the configured downloads path, so the result stays relative to the
/// WebDAV root and composes with `webdav_url` the same way a listed href does.
fn torrent_file_href(wdp: &str, info_hash: &str, relative_name: &str) -> String {
    format!(
        "{}/{}/{}",
        wdp.trim_matches('/'),
        info_hash,
        encode_path_segments(relative_name.trim_start_matches('/'))
    )
    .trim_start_matches('/')
    .to_string()
}

async fn find_file(
    http: &Client,
    cfg: &QbConfig,
    session_cookie: &str,
    info_hash: &str,
    torrent_name: &str,
    filename: Option<&str>,
    season: Option<i32>,
    episode: Option<i32>,
) -> Result<String, ProviderError> {
    // qBittorrent already knows every file of the torrent: relative path, size
    // and index. Selecting from that list needs no directory walking at all,
    // and two files sharing a basename in different directories stay distinct
    // because the whole relative path is what we match and build the URL from.
    let qb_files = qb_torrent_files(http, cfg, session_cookie, info_hash)
        .await
        .unwrap_or_default();
    if !qb_files.is_empty() {
        let files: Vec<FileEntry> = qb_files
            .iter()
            .map(|f| FileEntry {
                index: f.index,
                name: f.name.clone(),
                size: f.size,
            })
            .collect();
        if let Ok(idx) =
            select_torrent_file_index(&files, torrent_name, filename, season, episode, None, None)
        {
            for root in &cfg.downloads_paths {
                let href = torrent_file_href(root, info_hash, &files[idx].name);
                if webdav::exists(
                    http,
                    &cfg.webdav_url,
                    &href,
                    &cfg.webdav_user,
                    &cfg.webdav_pass,
                )
                .await
                {
                    return Ok(href);
                }
            }
        }
    }

    // Fallback for when qBittorrent does not know the hash: discover the layout
    // by walking the WebDAV tree instead.
    for root in &cfg.downloads_paths {
        let path = format!("{}/{}", root.trim_end_matches('/'), info_hash);
        let files = list_webdav_files_recursive(http, cfg, &path).await?;
        if files.is_empty() {
            continue;
        }
        let idx =
            select_torrent_file_index(&files, torrent_name, filename, season, episode, None, None)?;
        return Ok(files[idx].name.clone());
    }
    Err(ProviderError::api(
        "No matching file available for this torrent",
        "no_matching_file.mp4",
    ))
}

pub async fn validate_credentials(http: &Client, config: &Value) -> Result<(), ProviderError> {
    let cfg = parse_config(config)?;
    qb_login(http, &cfg).await?;
    webdav::list(
        http,
        &cfg.webdav_url,
        "",
        &cfg.webdav_user,
        &cfg.webdav_pass,
    )
    .await
    .map_err(|_| ProviderError::api("Invalid WebDAV credentials", "invalid_credentials.mp4"))?;
    Ok(())
}

pub async fn get_video_url(
    http: &Client,
    config: &Value,
    info_hash: &str,
    magnet_link: &str,
    torrent_name: &str,
    filename: Option<&str>,
    season: Option<i32>,
    episode: Option<i32>,
    torrent_file: Option<&[u8]>,
    is_private: bool,
) -> Result<String, ProviderError> {
    let cfg = parse_config(config)?;
    let session_cookie = qb_login(http, &cfg).await?;

    let existing_progress = qb_torrent_info(http, &cfg, &session_cookie, info_hash).await?;
    if existing_progress.is_none() {
        qb_add_torrent(
            http,
            &cfg,
            &session_cookie,
            magnet_link,
            info_hash,
            torrent_name,
            torrent_file,
            is_private,
        )
        .await?;
    } else if existing_progress.is_some_and(|p| p * 100.0 < cfg.play_video_after as f64) {
        wait_for_progress(http, &cfg, &session_cookie, info_hash).await?;
    }

    let file_path = match find_file(
        http,
        &cfg,
        &session_cookie,
        info_hash,
        torrent_name,
        filename,
        season,
        episode,
    )
    .await
    {
        Ok(p) => p,
        Err(_) => {
            qb_add_magnet(http, &cfg, &session_cookie, magnet_link, info_hash).await?;
            wait_for_progress(http, &cfg, &session_cookie, info_hash).await?;
            find_file(
                http,
                &cfg,
                &session_cookie,
                info_hash,
                torrent_name,
                filename,
                season,
                episode,
            )
            .await?
        }
    };

    Ok(webdav::url_with_creds(
        &cfg.webdav_url,
        &file_path,
        &cfg.webdav_user,
        &cfg.webdav_pass,
    ))
}

/// List info_hashes present as WebDAV download folders.
pub async fn list_downloaded_hashes(http: &Client, config: &Value) -> Vec<String> {
    let cfg = match parse_config(config) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut merged = std::collections::HashSet::new();
    for root in &cfg.downloads_paths {
        if let Ok(hrefs) = webdav::list(
            http,
            &cfg.webdav_url,
            root.trim_start_matches('/'),
            &cfg.webdav_user,
            &cfg.webdav_pass,
        )
        .await
        {
            for href in hrefs {
                let name = href.trim_end_matches('/');
                if name.len() == 40 && name.chars().all(|c| c.is_ascii_hexdigit()) {
                    merged.insert(name.to_string());
                }
            }
        }
    }
    merged.into_iter().collect()
}

pub async fn delete_all_torrents(http: &Client, config: &Value) -> Result<(), ProviderError> {
    let cfg = parse_config(config)?;
    let session_cookie = qb_login(http, &cfg).await?;
    let url = format!("{}/api/v2/torrents/info?filter=completed", cfg.qb_url);
    let arr: Vec<Value> = http
        .get(&url)
        .header(COOKIE, &session_cookie)
        .send()
        .await?
        .json()
        .await?;
    let hashes: Vec<String> = arr
        .iter()
        .filter_map(|t| t.get("hash").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    if hashes.is_empty() {
        return Ok(());
    }
    let del_url = format!("{}/api/v2/torrents/delete", cfg.qb_url);
    http.post(&del_url)
        .header(COOKIE, &session_cookie)
        .form(&[
            ("hashes", hashes.join("|")),
            ("deleteFiles", "true".to_string()),
        ])
        .send()
        .await?;
    Ok(())
}

/// Update cached flags by checking qBittorrent torrent progress == 1.0.
pub async fn update_cache_status(
    http: &Client,
    config: &Value,
    info_hashes: &[String],
) -> std::collections::HashMap<String, bool> {
    let cfg = match parse_config(config) {
        Ok(c) => c,
        Err(_) => return std::collections::HashMap::new(),
    };
    let session_cookie = match qb_login(http, &cfg).await {
        Ok(cookie) => cookie,
        Err(_) => return std::collections::HashMap::new(),
    };
    let joined = info_hashes.join("|");
    let url = format!("{}/api/v2/torrents/info?hashes={joined}", cfg.qb_url);
    let arr: Vec<Value> = match http.get(&url).header(COOKIE, session_cookie).send().await {
        Ok(r) => r.json().await.unwrap_or_default(),
        Err(_) => return std::collections::HashMap::new(),
    };
    let mut map = std::collections::HashMap::new();
    for t in arr {
        if let (Some(h), Some(p)) = (
            t.get("hash").and_then(|v| v.as_str()),
            t.get("progress").and_then(|v| v.as_f64()),
        ) {
            map.insert(h.to_lowercase(), (p - 1.0).abs() < f64::EPSILON);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use reqwest::header::{HeaderMap, HeaderValue, SET_COOKIE};
    use serde_json::json;

    use super::{
        DEFAULT_DOWNLOAD_WAIT_TIMEOUT_SECS, Href, QbFile, classify_href, encode_path_segments,
        parse_config, session_cookie, torrent_file_href,
    };

    #[test]
    fn download_wait_timeout_defaults_to_five_minutes() {
        let cfg = parse_config(&json!({
            "qbittorrent_url": "http://qb:8080",
            "webdav_url": "http://dav/webdav",
        }))
        .unwrap();

        assert_eq!(
            cfg.download_wait_timeout_secs as i64,
            DEFAULT_DOWNLOAD_WAIT_TIMEOUT_SECS
        );
    }

    #[test]
    fn download_wait_timeout_is_read_from_both_spellings() {
        for key in ["download_wait_timeout", "dwt"] {
            let cfg = parse_config(&json!({
                "qbittorrent_url": "http://qb:8080",
                "webdav_url": "http://dav/webdav",
                key: 1800,
            }))
            .unwrap();

            assert_eq!(cfg.download_wait_timeout_secs, 1800, "{key}");
        }
    }

    #[test]
    fn non_positive_download_wait_timeout_falls_back_to_the_default() {
        for value in [json!(0), json!(-5)] {
            let cfg = parse_config(&json!({
                "qbittorrent_url": "http://qb:8080",
                "webdav_url": "http://dav/webdav",
                "dwt": value,
            }))
            .unwrap();

            assert_eq!(
                cfg.download_wait_timeout_secs as i64,
                DEFAULT_DOWNLOAD_WAIT_TIMEOUT_SECS
            );
        }
    }

    #[test]
    fn play_video_after_is_clamped_to_a_percentage() {
        for (raw, expected) in [(json!(-10), 0), (json!(30), 30), (json!(250), 100)] {
            let cfg = parse_config(&json!({
                "qbittorrent_url": "http://qb:8080",
                "webdav_url": "http://dav/webdav",
                "pva": raw,
            }))
            .unwrap();

            assert_eq!(cfg.play_video_after, expected, "{raw}");
        }
    }

    #[test]
    fn classify_href_treats_a_trailing_slash_as_a_directory() {
        // The regression: the basename of ".../Some.Release/" is empty once the
        // slash is left in, so subdirectories were dropped and multi-file
        // torrents never resolved.
        assert!(matches!(
            classify_href("/abc/Some.Release/", "/abc"),
            Some(Href::Dir("Some.Release"))
        ));
        assert!(matches!(
            classify_href("/abc/movie.mkv", "/abc"),
            Some(Href::File("movie.mkv"))
        ));
        // Names containing dots or spaces must survive untouched.
        assert!(matches!(
            classify_href("/abc/Show.S01.2160p.WEB-DL/", "/abc"),
            Some(Href::Dir("Show.S01.2160p.WEB-DL"))
        ));
        assert!(matches!(
            classify_href("/abc/Ben The Men.mkv", "/abc"),
            Some(Href::File("Ben The Men.mkv"))
        ));
    }

    #[test]
    fn classify_href_skips_the_listed_collection_itself() {
        // PROPFIND echoes the collection as its first href; walking into it
        // would request a path that does not exist.
        assert!(classify_href("/abc/", "/abc").is_none());
        assert!(classify_href("/abc", "/abc").is_none());
        // `dir` is the path we requested (no leading slash), hrefs are absolute.
        assert!(classify_href("/abc/sub/", "abc/sub").is_none());
        assert!(classify_href("/abc/", "abc").is_none());
        // A sibling collection is still walked.
        assert!(classify_href("/abc/other/", "/abc").is_some());
        assert!(classify_href("/abc/other/", "abc").is_some());
    }

    #[test]
    fn torrent_file_href_builds_an_encoded_webdav_path() {
        // qBittorrent names are raw on-disk paths; a WebDAV href is encoded.
        assert_eq!(
            torrent_file_href("/", "abc123", "movie.mkv"),
            "abc123/movie.mkv"
        );
        assert_eq!(
            torrent_file_href("/", "abc123", "Show.S01/The.End[Ben The Men].mkv"),
            "abc123/Show.S01/The.End%5BBen%20The%20Men%5D.mkv"
        );
        // A configured subdirectory is kept, without a leading slash so it
        // composes with webdav_url the same way a listed href does.
        assert_eq!(
            torrent_file_href("/downloads", "abc123", "Sub/movie.mkv"),
            "downloads/abc123/Sub/movie.mkv"
        );
    }

    #[test]
    fn encode_path_segments_keeps_separators_and_sub_delims() {
        assert_eq!(
            encode_path_segments("Show.S01/Ben The Men.mkv"),
            "Show.S01/Ben%20The%20Men.mkv"
        );
        // A literal percent must not be able to smuggle an escape sequence in.
        assert_eq!(encode_path_segments("100%25.mkv"), "100%2525.mkv");
        assert_eq!(encode_path_segments("a+b,c;d=e@f.mkv"), "a+b,c;d=e@f.mkv");
    }

    #[test]
    fn extract_qbittorrent_sid_cookie() {
        let mut headers = HeaderMap::new();
        headers.append(SET_COOKIE, HeaderValue::from_static("theme=dark; Path=/"));
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("SID=authenticated-session; HttpOnly; Path=/"),
        );

        assert_eq!(
            session_cookie(&headers).as_deref(),
            Some("SID=authenticated-session")
        );
    }
}
