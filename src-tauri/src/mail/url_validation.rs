use crate::error::{Error, Result};

/// Reject URLs that would send credentials over cleartext.
///
/// Accepts `https://` URLs. In debug builds, `http://` is permitted for
/// loopback hosts (`localhost`, `127.0.0.0/8`, `::1`) to support local
/// development against test servers. Release builds reject all cleartext
/// URLs unconditionally.
pub fn require_https(url: &str) -> Result<()> {
    let parsed = url::Url::parse(url)
        .map_err(|e| Error::Other(format!("Invalid URL '{}': {}", url, e)))?;

    match parsed.scheme() {
        "https" => Ok(()),
        "http" if cfg!(debug_assertions) && is_loopback_host(parsed.host_str()) => Ok(()),
        scheme => Err(Error::Other(format!(
            "URL must use https:// (got '{}'): {}",
            scheme, url
        ))),
    }
}

fn is_loopback_host(host: Option<&str>) -> bool {
    let Some(host) = host else { return false };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    // Bracketed IPv6 literals arrive without brackets from url::Url::host_str,
    // but handle the raw-string case for safety.
    if let Some(stripped) = host.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        if let Ok(ip) = stripped.parse::<std::net::IpAddr>() {
            return ip.is_loopback();
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_https_urls() {
        assert!(require_https("https://example.com").is_ok());
        assert!(require_https("https://example.com:8443/path").is_ok());
        assert!(require_https("https://user@example.com").is_ok());
    }

    #[test]
    fn rejects_http_on_public_host() {
        let err = require_https("http://example.com").unwrap_err().to_string();
        assert!(err.contains("https"), "error should mention https: {}", err);
    }

    #[test]
    fn allows_http_loopback_localhost() {
        assert!(require_https("http://localhost").is_ok());
        assert!(require_https("http://localhost:8080/jmap").is_ok());
        assert!(require_https("http://LocalHost").is_ok());
    }

    #[test]
    fn allows_http_loopback_ipv4() {
        assert!(require_https("http://127.0.0.1").is_ok());
        assert!(require_https("http://127.0.0.1:8080").is_ok());
        assert!(require_https("http://127.1.2.3").is_ok());
    }

    #[test]
    fn allows_http_loopback_ipv6() {
        assert!(require_https("http://[::1]").is_ok());
        assert!(require_https("http://[::1]:8080").is_ok());
    }

    #[test]
    fn rejects_non_http_schemes() {
        assert!(require_https("ftp://example.com").is_err());
        assert!(require_https("file:///etc/passwd").is_err());
        assert!(require_https("javascript:alert(1)").is_err());
    }

    #[test]
    fn rejects_invalid_urls() {
        assert!(require_https("not a url").is_err());
        assert!(require_https("").is_err());
    }

    #[test]
    fn rejects_http_on_non_loopback_ip() {
        assert!(require_https("http://192.168.1.1").is_err());
        assert!(require_https("http://8.8.8.8").is_err());
        assert!(require_https("http://[2001:db8::1]").is_err());
    }
}
