/// WebDAV helpers shared by SABnzbd, NzbDAV, and NZBGet providers.
///
/// Uses a PROPFIND Depth:1 request to list the download directory, then
/// selects the best-matching video file by season/episode or size.
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use quick_xml::{Reader, events::Event};

use crate::providers::ProviderError;

use super::{is_video_name, jaccard_similarity};

/// Issue a WebDAV PROPFIND on `{webdav_base}/{dir_name}` and return all `<href>` values.
pub async fn list(
    http: &reqwest::Client,
    webdav_base: &str,
    dir_name: &str,
    username: &str,
    password: &str,
) -> Result<Vec<String>, ProviderError> {
    let url = if dir_name.is_empty() {
        webdav_base.trim_end_matches('/').to_string()
    } else {
        format!(
            "{}/{}",
            webdav_base.trim_end_matches('/'),
            urlencoding::encode(dir_name)
        )
    };

    let xml = http
        .request(
            reqwest::Method::from_bytes(b"PROPFIND")
                .map_err(|e| ProviderError::api(format!("PROPFIND method: {e}"), "webdav_error.mp4"))?,
            &url,
        )
        .header("Depth", "1")
        .header("Content-Type", "application/xml")
        .basic_auth(username, Some(password))
        .body(
            r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#,
        )
        .send()
        .await?
        .text()
        .await?;

    parse_hrefs(&xml)
}

/// Whether `href` (relative to the WebDAV root) resolves to something.
///
/// Used to confirm a path built from a provider's own file list actually landed
/// on disk before it is handed to the player.
pub async fn exists(
    http: &reqwest::Client,
    webdav_base: &str,
    href: &str,
    username: &str,
    password: &str,
) -> bool {
    let url = format!(
        "{}/{}",
        webdav_base.trim_end_matches('/'),
        href.trim_start_matches('/')
    );
    let method = reqwest::Method::from_bytes(b"PROPFIND").unwrap_or(reqwest::Method::GET);
    let resp = http
        .request(method, &url)
        .header("Depth", "0")
        .basic_auth(username, Some(password))
        .send()
        .await;
    match resp {
        Ok(r) => {
            let status = r.status();
            status.is_success() || status == reqwest::StatusCode::MULTI_STATUS
        }
        Err(_) => false,
    }
}

fn parse_hrefs(xml: &str) -> Result<Vec<String>, ProviderError> {
    let mut hrefs: Vec<String> = Vec::new();
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut in_href = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) if e.local_name().as_ref() == b"href" => {
                in_href = true;
            }
            Ok(Event::Text(ref e)) if in_href => {
                if let Ok(text) = e.decode() {
                    let s = text.trim().to_string();
                    if !s.is_empty() {
                        hrefs.push(s);
                    }
                }
                in_href = false;
            }
            Ok(Event::End(ref e)) if e.local_name().as_ref() == b"href" => {
                in_href = false;
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(ProviderError::api(
                    format!("WebDAV XML parse error: {e}"),
                    "webdav_error.mp4",
                ));
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(hrefs)
}

/// Pick the best video file from a WebDAV listing.
///
/// Prefers files matching `S{season}E{episode}`, then falls back to the
/// video file whose path most closely resembles `stream_name` by Jaccard
/// similarity, breaking ties by path length.
pub fn select_video(
    hrefs: &[String],
    stream_name: &str,
    season: i32,
    episode: i32,
) -> Option<String> {
    let videos: Vec<&str> = hrefs
        .iter()
        .map(|h| h.as_str())
        .filter(|h| is_video_name(h))
        .collect();

    if videos.is_empty() {
        return None;
    }

    if season > 0 && episode > 0 {
        let se = format!("s{:02}e{:02}", season, episode);
        if let Some(v) = videos.iter().find(|v| v.to_lowercase().contains(&se)) {
            return Some((*v).to_string());
        }
    }

    let name_lc = stream_name.to_lowercase();
    videos
        .iter()
        .max_by_key(|v| {
            let sim = (jaccard_similarity(&v.to_lowercase(), &name_lc) * 1000.0) as i64;
            (sim, v.len() as i64)
        })
        .map(|s| (*s).to_string())
}

/// Characters that may not appear literally in the RFC 3986 `userinfo`
/// component, which is `*( unreserved / pct-encoded / sub-delims / ":" )`.
/// Everything else — letters, digits, `-._~` and the sub-delims
/// ``!$&'()*+,;=`` plus `:` — is left alone.
///
/// Two details matter here. `*` is a sub-delim and stays literal;
/// `encodeURIComponent` used to turn it into `%2A`, and players that pass
/// userinfo through without decoding it then sent the wrong password and got
/// a 401. And `%` has to be escaped explicitly, because it is the escape
/// character itself: leaving it bare means a client that *does* decode
/// userinfo turns a password containing `%25` into `%`, silently wrong.
const USERINFO: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}');

/// Percent-encode a credential for embedding in a URL's userinfo component.
fn encode_userinfo(value: &str) -> String {
    utf8_percent_encode(value, USERINFO).to_string()
}

/// Build a WebDAV URL with `user:pass@` embedded in the authority.
pub fn url_with_creds(
    webdav_base: &str,
    file_path: &str,
    username: &str,
    password: &str,
) -> String {
    let enc_u = encode_userinfo(username);
    let enc_p = encode_userinfo(password);
    let file = file_path.trim_start_matches('/');
    if let Some(rest) = webdav_base.strip_prefix("https://") {
        format!(
            "https://{enc_u}:{enc_p}@{}/{file}",
            rest.trim_end_matches('/')
        )
    } else if let Some(rest) = webdav_base.strip_prefix("http://") {
        format!(
            "http://{enc_u}:{enc_p}@{}/{file}",
            rest.trim_end_matches('/')
        )
    } else {
        format!("{}/{file}", webdav_base.trim_end_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use percent_encoding::percent_decode_str;

    use super::{encode_userinfo, url_with_creds};

    /// What a client without userinfo decoding would send as the Basic password.
    fn naive_basic_password(url: &str) -> String {
        let after_scheme = url.split_once("://").expect("scheme").1;
        let userinfo = after_scheme.split_once('@').expect("userinfo").0;
        let (_user, pass) = userinfo.split_once(':').expect("user:pass");
        pass.to_string()
    }

    /// What a conformant client sends: percent-decoded userinfo.
    fn decoded_basic_password(url: &str) -> String {
        let naive = naive_basic_password(url);
        percent_decode_str(&naive).decode_utf8_lossy().into_owned()
    }

    #[test]
    fn star_and_sub_delims_stay_literal() {
        // '*' is a sub-delim and legal in userinfo. encodeURIComponent turned it
        // into %2A, which players that skip userinfo decoding sent verbatim.
        // ':' is legal in userinfo too, and the password is whatever follows the
        // first colon, so it needs no escaping either.
        for raw in ["a*b*c", "x!y$z&w'v(u)t,s;q=r", "pass:with:colons"] {
            assert_eq!(encode_userinfo(raw), raw, "{raw}");
        }
    }

    #[test]
    fn structurally_significant_characters_are_escaped() {
        assert_eq!(encode_userinfo("a@b"), "a%40b");
        assert_eq!(encode_userinfo("a/b"), "a%2Fb");
        assert_eq!(encode_userinfo("a b"), "a%20b");
        assert_eq!(encode_userinfo("a?b"), "a%3Fb");
        assert_eq!(encode_userinfo("a#b"), "a%23b");
        assert_eq!(encode_userinfo("a[b]"), "a%5Bb%5D");
        assert_eq!(encode_userinfo("a\\b"), "a%5Cb");
    }

    #[test]
    fn percent_is_escaped_so_decoding_round_trips() {
        // '%' is the escape character itself, so leaving it bare would let a
        // decoding client mangle a password that happens to contain "%25".
        assert_eq!(encode_userinfo("a%b"), "a%25b");
        assert_eq!(encode_userinfo("50%"), "50%25");

        for raw in ["Vd*p3hEc*VcKVV%", "a%b", "a%25b", "plain", "p@ss w:rd#1"] {
            let url = url_with_creds("http://dav/webdav", "/f.mkv", "u", raw);
            assert_eq!(decoded_basic_password(&url), raw, "{raw}");
        }
    }

    #[test]
    fn unreserved_passwords_need_no_encoding_at_all() {
        // The recommended shape: nothing is escaped, so naive and decoding
        // clients agree and neither can get it wrong.
        for raw in [
            "kQw7Zr4Tn9Xb2Vm6L",
            "kQw7Zr4Tn9Xb2-Vm6L",
            "kQw7Zr4Tn9Xb2_Vm6L",
        ] {
            let url = url_with_creds("http://dav/webdav", "/f.mkv", "marco", raw);

            assert_eq!(naive_basic_password(&url), raw, "{raw}");
            assert_eq!(decoded_basic_password(&url), raw, "{raw}");
            assert!(!url.contains('%'), "{url}");
        }
    }

    #[test]
    fn star_password_works_for_clients_that_skip_userinfo_decoding() {
        // The regression: encodeURIComponent produced %2A here, so such a client
        // authenticated with "Vd%2A..." and the server answered 401.
        let password = "Vd*p3hEc*VcKVV";
        let url = url_with_creds("http://dav/webdav", "/f.mkv", "marco", password);

        assert_eq!(naive_basic_password(&url), password);
        assert!(!url.contains("%2A"), "{url}");
    }

    #[test]
    fn credentials_survive_a_conformant_client() {
        let password = "p@ss w:rd#1";
        let url = url_with_creds("http://dav/webdav", "/f.mkv", "us er", password);

        assert_eq!(decoded_basic_password(&url), password);
    }

    #[test]
    fn url_keeps_scheme_host_and_path_intact() {
        let url = url_with_creds(
            "https://mediafusion.example/webdav/",
            "/some/dir/file.mkv",
            "marco",
            "kQw7Zr4Tn9Xb2-Vm6L",
        );

        assert_eq!(
            url,
            "https://marco:kQw7Zr4Tn9Xb2-Vm6L@mediafusion.example/webdav/some/dir/file.mkv"
        );
    }

    #[test]
    fn plain_credentials_are_untouched() {
        let url = url_with_creds("http://dav/webdav", "/f.mkv", "user", "pass");

        assert_eq!(url, "http://user:pass@dav/webdav/f.mkv");
    }
}
