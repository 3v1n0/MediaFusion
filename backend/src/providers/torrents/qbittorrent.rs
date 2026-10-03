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

/// Upper bound on waiting for qBittorrent to fetch a magnet's metadata.
const METADATA_WAIT_SECS: u64 = 20;

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
    /// Ask qBittorrent for the head and tail of the file first, so playback can
    /// start long before the rest of it arrives.
    first_last_piece_prio: bool,
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
    let first_last_piece_prio = raw
        .get("first_last_piece_prio")
        .or_else(|| raw.get("flpp"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
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
        first_last_piece_prio,
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
    /// 0.0..=1.0 for this file alone, unlike `torrents/info`'s whole-torrent number.
    progress: f64,
}

/// What `torrents/info` reports about the torrent as a whole.
///
/// Its `progress` is deliberately not kept: it averages over every file, so a
/// season pack reads as finished while the episode being played has barely
/// started. Use the per-file number from `torrents/files` instead.
struct QbTorrent {
    state: String,
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
                progress: v.get("progress").and_then(|p| p.as_f64()).unwrap_or(0.0),
            })
        })
        .collect())
}

/// Project qBittorrent's file list onto the selector's view of it.
fn file_entries_of(qb_files: &[QbFile]) -> Vec<FileEntry> {
    qb_files
        .iter()
        .map(|f| FileEntry {
            index: f.index,
            name: f.name.clone(),
            size: f.size,
        })
        .collect()
}

/// Tell qBittorrent to download only `wanted`, skipping every other file.
///
/// A torrent is added with `sequentialDownload`, so without this qBittorrent
/// fetches the whole torrent from index 0 and a season pack puts the episode
/// that was asked for last. `id` is the file index, which equals the position
/// in the list we were given, and qBittorrent splits that list on `|`.
async fn qb_select_only_file(
    http: &Client,
    cfg: &QbConfig,
    session_cookie: &str,
    info_hash: &str,
    files: &[QbFile],
    wanted: usize,
) -> Result<(), ProviderError> {
    if files.len() <= 1 {
        return Ok(());
    }
    let others: Vec<String> = (0..files.len())
        .filter(|i| *i != wanted)
        .map(|i| i.to_string())
        .collect();
    if others.is_empty() {
        return Ok(());
    }
    let others = others.join("|");
    let url = format!("{}/api/v2/torrents/filePrio", cfg.qb_url);
    let resp = http
        .post(&url)
        .header(COOKIE, session_cookie)
        .form(&[
            ("hash", info_hash),
            ("id", others.as_str()),
            ("priority", "0"),
        ])
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default().to_lowercase();
    if status.is_success() {
        return Ok(());
    }
    Err(ProviderError::api(
        format!("qBittorrent refused to skip files: {text}"),
        "add_torrent_failed.mp4",
    ))
}

/// qBittorrent reports no files until it has the metadata, which for a magnet
/// means after its own metadata fetch. Kept short so an unknown hash still
/// falls through to the WebDAV walk instead of burning the whole budget.
async fn qb_wait_for_files(
    http: &Client,
    cfg: &QbConfig,
    session_cookie: &str,
    info_hash: &str,
) -> Option<Vec<QbFile>> {
    let budget = cfg.download_wait_timeout_secs.clamp(1, METADATA_WAIT_SECS);
    let deadline = Instant::now() + Duration::from_secs(budget);
    loop {
        let files = qb_torrent_files(http, cfg, session_cookie, info_hash)
            .await
            .unwrap_or_default();
        if !files.is_empty() {
            return Some(files);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Wait for the one file we selected, not the torrent as a whole: a season pack
/// reports its progress as the average over every file, so the torrent can read
/// as done while the episode we want has barely started.
/// Wait until the file we picked is both at the configured progress and on
/// disk, since WebDAV can only serve what is actually there.
///
/// The database still lists a torrent as downloaded after its data is removed,
/// and qBittorrent keeps reporting such a torrent as complete. When the file is
/// missing at full progress, recheck so the missing pieces are wanted again and
/// the wait carries on over the re-download instead of failing outright.
async fn qb_wait_until_playable(
    http: &Client,
    cfg: &QbConfig,
    session_cookie: &str,
    info_hash: &str,
    wanted: usize,
    href: &str,
) -> Result<(), ProviderError> {
    let threshold = cfg.play_video_after as f64 / 100.0;
    let deadline = Instant::now() + Duration::from_secs(cfg.download_wait_timeout_secs);
    let mut last_progress = 0.0f64;
    let mut rechecked = false;

    loop {
        if let Ok(files) = qb_torrent_files(http, cfg, session_cookie, info_hash).await
            && let Some(f) = files.get(wanted)
        {
            last_progress = f.progress;
            if f.progress >= threshold
                && webdav::exists(
                    http,
                    &cfg.webdav_url,
                    href,
                    &cfg.webdav_user,
                    &cfg.webdav_pass,
                )
                .await
            {
                return Ok(());
            }
            if f.progress >= threshold && !rechecked {
                rechecked = true;
                tracing::debug!(
                    info_hash = %info_hash,
                    "qBittorrent calls the file complete but WebDAV does not have it; rechecking"
                );
                qb_post(
                    http,
                    cfg,
                    session_cookie,
                    "/api/v2/torrents/recheck",
                    info_hash,
                )
                .await?;
            }
        }

        if Instant::now() >= deadline {
            break;
        }
        // A stopped or paused torrent never advances, so keep nudging it.
        if let Ok(Some(status)) = qb_torrent_info(http, cfg, session_cookie, info_hash).await {
            let _ = qb_ensure_running(http, cfg, session_cookie, info_hash, &status).await;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    tracing::debug!(
        info_hash = %info_hash,
        progress_pct = %(last_progress * 100.0),
        target_pct = %cfg.play_video_after,
        waited_secs = %cfg.download_wait_timeout_secs,
        "qBittorrent file download wait timed out"
    );
    Err(ProviderError::api(
        format!(
            "Selected file at {:.1}% after {}s, still below the configured {}% — retry once it downloads further",
            last_progress * 100.0,
            cfg.download_wait_timeout_secs,
            cfg.play_video_after
        ),
        "torrent_not_downloaded.mp4",
    ))
}

/// POST one of qBittorrent's torrent actions, addressing the torrent by hash.
async fn qb_post(
    http: &Client,
    cfg: &QbConfig,
    session_cookie: &str,
    endpoint: &str,
    info_hash: &str,
) -> Result<(), ProviderError> {
    let url = format!("{}{endpoint}", cfg.qb_url);
    let resp = http
        .post(&url)
        .header(COOKIE, session_cookie)
        .form(&[("hashes", info_hash)])
        .send()
        .await?;
    if resp.status().is_success() {
        return Ok(());
    }
    let text = resp.text().await.unwrap_or_default().to_lowercase();
    Err(ProviderError::api(
        format!("qBittorrent {endpoint} failed: {text}"),
        "torrent_not_downloaded.mp4",
    ))
}

async fn qb_torrent_info(
    http: &Client,
    cfg: &QbConfig,
    session_cookie: &str,
    info_hash: &str,
) -> Result<Option<QbTorrent>, ProviderError> {
    let url = format!("{}/api/v2/torrents/info?hashes={info_hash}", cfg.qb_url);
    let arr: Vec<Value> = http
        .get(&url)
        .header(COOKIE, session_cookie)
        .send()
        .await?
        .json()
        .await?;
    Ok(arr.first().map(|t| QbTorrent {
        state: t
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
    }))
}

/// qBittorrent keeps a torrent paused or stopped once its ratio/time limits are
/// reached, and reports one whose data was deleted as `missingFiles`. Either
/// way the bytes never arrive on their own, so playback has to restart it.
async fn qb_ensure_running(
    http: &Client,
    cfg: &QbConfig,
    session_cookie: &str,
    info_hash: &str,
    status: &QbTorrent,
) -> Result<(), ProviderError> {
    let (endpoint, matches_state) = match status.state.as_str() {
        "stopped" | "paused" | "pausedDL" | "pausedUP" => ("/api/v2/torrents/start", true),
        "missingFiles" => ("/api/v2/torrents/recheck", true),
        "error" => ("/api/v2/torrents/recheck", true),
        _ => return Ok(()),
    };
    if !matches_state {
        return Ok(());
    }

    tracing::debug!(info_hash = %info_hash, state = %status.state, "restarting torrent");
    let url = format!("{}{endpoint}", cfg.qb_url);
    let resp = http
        .post(&url)
        .header(COOKIE, session_cookie)
        .form(&[("hashes", info_hash)])
        .send()
        .await?;
    if resp.status().is_success() {
        return Ok(());
    }
    let text = resp.text().await.unwrap_or_default().to_lowercase();
    Err(ProviderError::api(
        format!("qBittorrent could not restart the torrent: {text}"),
        "torrent_not_downloaded.mp4",
    ))
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
                    .text(
                        "firstLastPiecePrio",
                        if cfg.first_last_piece_prio {
                            "true"
                        } else {
                            "false"
                        },
                    )
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
                (
                    "firstLastPiecePrio",
                    if cfg.first_last_piece_prio {
                        "true"
                    } else {
                        "false"
                    },
                ),
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
    let qb_files = qb_wait_for_files(http, cfg, session_cookie, info_hash).await;
    if let Some(qb_files) = qb_files {
        let files = file_entries_of(&qb_files);
        if let Ok(idx) =
            select_torrent_file_index(&files, torrent_name, filename, season, episode, None, None)
        {
            // Only this file, so a season pack does not queue the whole run.
            qb_select_only_file(http, cfg, session_cookie, info_hash, &qb_files, idx).await?;

            for root in &cfg.downloads_paths {
                let href = torrent_file_href(root, info_hash, &files[idx].name);
                if qb_wait_until_playable(http, cfg, session_cookie, info_hash, idx, &href)
                    .await
                    .is_ok()
                {
                    return Ok(href);
                }
            }
            // Falling through here would re-select from what happens to be on
            // disk and hand back a different episode than the one requested.
            return Err(ProviderError::api(
                "The selected file is not on disk yet",
                "torrent_not_downloaded.mp4",
            ));
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

    let existing = qb_torrent_info(http, &cfg, &session_cookie, info_hash).await?;
    if let Some(status) = existing.as_ref() {
        // A torrent that finished, was stopped or lost its data never advances
        // on its own, so restart it before waiting on it.
        qb_ensure_running(http, &cfg, &session_cookie, info_hash, status).await?;
    }
    if existing.is_none() {
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
    }

    // find_file waits for the file it picked, so a multi-file torrent is judged
    // on the episode being played rather than on the whole pack's average.
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
        Err(e) => {
            if existing.is_some() {
                // Already known and the add would be a no-op, so the failure is
                // real: no matching file, or it never finished downloading.
                return Err(e);
            }
            qb_add_magnet(http, &cfg, &session_cookie, magnet_link, info_hash).await?;
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
        file_entries_of, parse_config, select_torrent_file_index, session_cookie,
        torrent_file_href,
    };

    #[test]
    fn season_pack_picks_the_requested_episode_not_the_first_file() {
        // Real layout from a White Lotus season pack: one directory, many files.
        let dir = "The White Lotus Season 1 (S01) 2160p HDR 5.1 - 2.0 x265 10bit Phun Psyz";
        let episodes = [
            ("The White Lotus S01E01 Arrivals.mkv", 1_224_299_892),
            (
                "The White Lotus S01E02 Establishing Canada.mkv",
                1_180_004_096,
            ),
            ("The White Lotus S01E03 Making Friends.mkv", 1_310_002_944),
            ("The White Lotus S01E04 New Design.mkv", 1_095_000_000),
        ];
        let qb_files: Vec<QbFile> = episodes
            .iter()
            .enumerate()
            .map(|(i, (name, size))| QbFile {
                index: i,
                name: format!("{dir}/{name}"),
                size: *size,
                // Only the first file is on disk, exactly the state that used to
                // make playback resolve to the wrong episode.
                progress: if i == 0 { 1.0 } else { 0.0 },
            })
            .collect();

        let files = file_entries_of(&qb_files);
        let idx = select_torrent_file_index(
            &files,
            "The White Lotus Season 1",
            None,
            Some(1),
            Some(3),
            None,
            None,
        )
        .expect("S01E03 should be selectable");

        assert_eq!(idx, 2);
        assert_eq!(
            qb_files[idx].name,
            format!("{dir}/The White Lotus S01E03 Making Friends.mkv")
        );
    }

    #[test]
    fn season_pack_picks_by_episode_regardless_of_the_filename_order() {
        let mk = |names: &[&str]| -> Vec<QbFile> {
            names
                .iter()
                .enumerate()
                .map(|(i, name)| QbFile {
                    index: i,
                    name: (*name).to_string(),
                    size: 1_000_000_000,
                    progress: 0.0,
                })
                .collect()
        };

        // Air order that does not match the listing order.
        let files = file_entries_of(&mk(&[
            "Show.S01E10.mkv",
            "Show.S01E02.mkv",
            "Show.S01E03.mkv",
        ]));
        let idx = select_torrent_file_index(&files, "Show", None, Some(1), Some(3), None, None)
            .expect("S01E03 should be selectable");
        assert_eq!(files[idx].name, "Show.S01E03.mkv");
    }

    #[test]
    fn first_last_piece_prio_is_on_by_default_for_streaming() {
        // qBittorrent defaults this to false when the parameter is absent, so
        // omitting it silently disabled the head-of-file-first behaviour.
        let cfg = parse_config(&json!({
            "qbittorrent_url": "http://qb:8080",
            "webdav_url": "http://dav/webdav",
        }))
        .unwrap();
        assert!(cfg.first_last_piece_prio);

        for key in ["first_last_piece_prio", "flpp"] {
            let cfg = parse_config(&json!({
                "qbittorrent_url": "http://qb:8080",
                "webdav_url": "http://dav/webdav",
                key: false,
            }))
            .unwrap();
            assert!(!cfg.first_last_piece_prio, "{key} should be read");
        }
    }

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
