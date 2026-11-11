use std::{
    env::VarError,
    error::Error,
    io::{Error as IoError, ErrorKind},
};

use reqwest::{Proxy, Url};
use teloxide::{Bot, net::default_reqwest_settings};

pub fn create_bot(
    token: impl Into<String>,
    proxy: Option<&str>,
) -> Result<Bot, Box<dyn Error + Send + Sync>> {
    let proxy = configured_proxy(proxy, || std::env::var("TELOXIDE_PROXY"))?;
    let mut builder = default_reqwest_settings();
    if let Some(proxy) = proxy {
        builder = builder.proxy(proxy);
    }
    let client = builder
        .build()
        .map_err(|_| IoError::other("failed to build the Telegram HTTP client"))?;
    Ok(Bot::with_client(token, client))
}

fn configured_proxy(
    explicit: Option<&str>,
    environment: impl FnOnce() -> Result<String, VarError>,
) -> Result<Option<Proxy>, IoError> {
    match explicit {
        Some(proxy) => parse_proxy(proxy, "Telegram proxy").map(Some),
        None => match environment() {
            Ok(proxy) => parse_proxy(&proxy, "TELOXIDE_PROXY").map(Some),
            Err(VarError::NotPresent) => Ok(None),
            Err(VarError::NotUnicode(_)) => Err(IoError::new(
                ErrorKind::InvalidInput,
                "TELOXIDE_PROXY must be a valid Unicode proxy URL",
            )),
        },
    }
}

fn parse_proxy(value: &str, setting: &str) -> Result<Proxy, IoError> {
    let invalid = || {
        IoError::new(
            ErrorKind::InvalidInput,
            format!("{setting} must be a nonblank HTTP(S) or SOCKS proxy URL with a host"),
        )
    };
    if value.trim().is_empty() {
        return Err(invalid());
    }
    let url = Url::parse(value).map_err(|_| invalid())?;
    if url.host_str().is_none()
        || !matches!(
            url.scheme(),
            "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
        )
    {
        return Err(invalid());
    }
    Proxy::all(url).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    #[test]
    fn explicit_proxy_skips_environment_lookup() {
        let proxy = configured_proxy(Some("http://proxy.example:8080"), || {
            panic!("explicit proxy must override TELOXIDE_PROXY")
        })
        .unwrap();
        assert!(proxy.is_some());
    }

    #[test]
    fn invalid_explicit_proxy_does_not_fall_back_to_environment() {
        let error = configured_proxy(Some(""), || {
            panic!("invalid explicit proxy must not fall back to TELOXIDE_PROXY")
        })
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn environment_proxy_is_used_when_no_override_is_provided() {
        let proxy = configured_proxy(None, || Ok("socks5h://proxy.example:1080".into())).unwrap();
        assert!(proxy.is_some());
    }

    #[test]
    fn absent_configuration_leaves_system_proxy_selection_to_reqwest() {
        assert!(
            configured_proxy(None, || Err(VarError::NotPresent))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn accepts_supported_proxy_urls() {
        for proxy in [
            "http://proxy.example:8080",
            "https://proxy-user:proxy-password@proxy.example:8443",
            "http://127.0.0.1:8080",
            "http://[::1]:8080",
            "socks4://proxy.example",
            "socks4a://proxy.example:1080",
            "socks5://proxy.example:1080",
            "socks5h://proxy-user:proxy-password@proxy.example",
        ] {
            assert!(parse_proxy(proxy, "Telegram proxy").is_ok());
        }
    }

    #[test]
    fn invalid_proxy_errors_do_not_disclose_credentials_or_input() {
        for proxy in [
            "",
            " \t\n",
            "proxy.example:8080",
            "//proxy.example:8080",
            "http://",
            "socks5:///",
            "socks5h://proxy-user:proxy-password@",
            "http://proxy-user:proxy-password@proxy.example:invalid-port",
            "ftp://proxy-user:proxy-password@proxy.example",
        ] {
            let error = parse_proxy(proxy, "Telegram proxy").unwrap_err();
            let message = format!("{error} {error:?}");
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            assert!(message.contains("Telegram proxy"));
            assert!(!message.contains("proxy-user"));
            assert!(!message.contains("proxy-password"));
            assert!(!message.contains("proxy.example"));
            assert!(error.source().is_none());
        }
    }

    #[test]
    fn invalid_environment_proxy_names_the_setting_without_disclosing_its_value() {
        let error = configured_proxy(None, || {
            Ok("ftp://proxy-user:proxy-password@proxy.example".into())
        })
        .unwrap_err();
        assert!(error.to_string().contains("TELOXIDE_PROXY"));
        assert!(!format!("{error:?}").contains("proxy-password"));
    }

    #[test]
    fn non_unicode_environment_proxy_is_a_sanitized_error() {
        let error = configured_proxy(None, || {
            Err(VarError::NotUnicode(OsString::from("private-proxy-value")))
        })
        .unwrap_err();
        assert!(error.to_string().contains("TELOXIDE_PROXY"));
        assert!(!format!("{error:?}").contains("private-proxy-value"));
    }
}
