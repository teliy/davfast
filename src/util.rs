//! Small shared helpers: path normalization, URL path encoding, XML escaping,
//! HTTP-date conversion and the mail.ru "modified SHA-1" file hash.

use anyhow::Result;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use sha1::{Digest, Sha1};

/// Characters allowed unescaped in a WebDAV URL path segment.
/// Everything else (cyrillic, spaces, `#`, `?`, `%`, ...) gets percent-encoded.
const PATH_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b'!');

/// Normalize a remote (WebDAV / cloud) path:
/// - always starts with `/`
/// - never ends with `/` (except the root `/`)
/// - collapses `//`, resolves `.` and `..` segments
pub fn normalize_remote(p: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for seg in p.trim().split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            s => stack.push(s),
        }
    }
    if stack.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", stack.join("/"))
    }
}

/// Percent-encode each path segment of an already-normalized remote path.
/// Input may itself contain percent-escapes — they are decoded first, so
/// double-encoding is impossible.
pub fn encode_dav_path(p: &str) -> String {
    let decoded = percent_decode_str(p).decode_utf8_lossy();
    decoded
        .split('/')
        .map(|seg| utf8_percent_encode(seg, PATH_SET).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// Decode a href coming back from a PROPFIND multistatus document into a plain
/// path (`/dir/file.txt`). Absolute URLs are reduced to their path part.
pub fn href_to_path(href: &str) -> String {
    let raw = if href.starts_with("http://") || href.starts_with("https://") {
        url::Url::parse(href)
            .map(|u| u.path().to_string())
            .unwrap_or_else(|_| href.to_string())
    } else {
        href.to_string()
    };
    percent_decode_str(&raw)
        .decode_utf8_lossy()
        .trim_end_matches('/')
        .to_string()
}

/// Last path segment ("file.txt"); empty string for the root.
pub fn name_from_path(p: &str) -> String {
    p.rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Escape a string for inclusion into XML text / attribute values.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Convert a unix timestamp into an RFC 1123 / HTTP date string
/// (`Wed, 21 Oct 2015 07:28:00 GMT`).
pub fn epoch_to_http_date(epoch: i64) -> String {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const MDAYS: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let mut year = 1970i64;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let ydays = if leap { 366 } else { 365 };
        if days >= ydays {
            days -= ydays;
            year += 1;
        } else {
            break;
        }
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let mut month = 0usize;
    loop {
        let mut md = MDAYS[month];
        if leap && month == 1 {
            md += 1;
        }
        if days >= md {
            days -= md;
            month += 1;
        } else {
            break;
        }
    }
    // 1970-01-01 was Thursday; DAYS[] starts on Monday
    let wd = (epoch.div_euclid(86_400).rem_euclid(7) + 3) as usize % 7;
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS[wd],
        days + 1,
        MONTHS[month],
        year,
        h,
        m,
        s
    )
}

/// mail.ru file hash ("modified SHA-1", compatible with the official web
/// client and rclone):
/// - files larger than 20 bytes: `sha1(data || decimal_size)`
/// - files up to 20 bytes: `sha1(data || zero padding up to 20 bytes)`
/// Returned as lowercase hex.
pub fn mailru_file_hash(data: &[u8]) -> String {
    let mut h = Sha1::new();
    if data.len() > 20 {
        h.update(data);
        h.update(data.len().to_string().as_bytes());
    } else {
        h.update(data);
        for _ in data.len()..20 {
            h.update([0u8]);
        }
    }
    hex::encode(h.finalize())
}

/// Parse the uploader response `"<hash>;<size>"`.
pub fn parse_upload_response(text: &str) -> Result<(String, u64)> {
    let t = text.trim();
    let mut parts = t.split(';');
    let hash = parts.next().unwrap_or("").trim().to_string();
    let size_str = parts.next().unwrap_or("").trim();
    if hash.is_empty() || size_str.is_empty() {
        anyhow::bail!("unexpected uploader response: {:?}", text);
    }
    let size: u64 = size_str
        .parse()
        .map_err(|_| anyhow::anyhow!("bad size in uploader response: {:?}", text))?;
    Ok((hash, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize() {
        assert_eq!(normalize_remote("/a//b/"), "/a/b");
        assert_eq!(normalize_remote("/"), "/");
        assert_eq!(normalize_remote("a/b"), "/a/b");
        assert_eq!(normalize_remote("/a/../b"), "/b");
        assert_eq!(normalize_remote("/.."), "/");
    }

    #[test]
    fn encode() {
        assert_eq!(
            encode_dav_path("/a b/файл.txt"),
            "/a%20b/%D1%84%D0%B0%D0%B9%D0%BB.txt"
        );
        // pre-encoded input must not be double encoded
        assert_eq!(encode_dav_path("/a%20b/c"), "/a%20b/c");
        assert_eq!(encode_dav_path("/"), "/");
    }

    #[test]
    fn hrefs() {
        assert_eq!(href_to_path("/dav/dir/file.txt"), "/dav/dir/file.txt");
        assert_eq!(href_to_path("/dav/dir/"), "/dav/dir");
        assert_eq!(href_to_path("https://example.com/dav/%D1%84.txt"), "/dav/ф.txt");
        assert_eq!(name_from_path("/dav/dir/file.txt"), "file.txt");
        assert_eq!(name_from_path("/"), "");
    }

    #[test]
    fn xml_escaping() {
        assert_eq!(xml_escape("a<b>&\"x\""), "a&lt;b&gt;&amp;&quot;x&quot;");
    }

    #[test]
    fn http_dates() {
        assert_eq!(epoch_to_http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(
            epoch_to_http_date(1_446_362_880),
            "Sun, 01 Nov 2015 07:28:00 GMT"
        );
        assert_eq!(
            epoch_to_http_date(1_700_000_000),
            "Tue, 14 Nov 2023 22:13:20 GMT"
        );
        // round trip through the httpdate parser
        for epoch in [0i64, 951_782_400, 1_446_362_880, 1_700_000_000, 4_102_444_800] {
            let s = epoch_to_http_date(epoch);
            let back = httpdate::parse_http_date(&s).unwrap();
            assert_eq!(
                back.duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64,
                epoch
            );
        }
    }

    #[test]
    fn upload_response() {
        let (h, s) = parse_upload_response("ABCDEF0123;12345").unwrap();
        assert_eq!(h, "ABCDEF0123");
        assert_eq!(s, 12345);
        assert!(parse_upload_response("garbage").is_err());
    }

    #[test]
    fn mailru_hash_shape() {
        let h = mailru_file_hash(b"hello");
        assert_eq!(h.len(), 40);
        let h2 = mailru_file_hash(&vec![7u8; 4096]);
        assert_eq!(h2.len(), 40);
        // deterministic
        assert_eq!(h, mailru_file_hash(b"hello"));
    }
}
