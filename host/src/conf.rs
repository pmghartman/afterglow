//! afterglow.conf: `key = value` lines, `#` comments, as Apollo's config and the
//! client's `confText` write it. Keys afterglow doesn't read are ignored, so a config
//! written for the C++ host still starts this one.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct Conf {
    /// the link's port on 127.0.0.1
    pub port: u16,
    /// what a driver must say first
    pub token: String,
    pub log_path: Option<PathBuf>,
    pub log_level: crate::log::Level,
    /// an app's virtual display made the primary one while it streams, so its windows open there
    pub primary: bool,
    /// the GPU to capture, encode and render the virtual display on, by its name; any when empty
    pub adapter: String,
    /// `software`: no hardware encoder tried
    pub encoder: String,
    /// Linux: the Xvfb that gives an app a display of its own; `Xvfb` on the PATH when empty
    pub xvfb: String,
    /// served on stdin and stdout for one app, not on a port for any
    pub stdio: bool,
}

impl Conf {
    /// for `--stdio`: no port, no token (whatever started us let the driver in)
    pub fn stdio() -> Conf {
        Conf {
            port: 0,
            token: String::new(),
            log_path: None,
            log_level: crate::log::Level::Warning,
            primary: false,
            adapter: String::new(),
            encoder: std::env::var("AFTERGLOW_ENCODER").unwrap_or_default(),
            xvfb: std::env::var("AFTERGLOW_XVFB").unwrap_or_default(),
            stdio: true,
        }
    }
}

pub fn parse(text: &str) -> Result<Conf, String> {
    let mut c = Conf {
        port: 0,
        token: String::new(),
        log_path: None,
        log_level: crate::log::Level::Info,
        primary: false,
        adapter: String::new(),
        encoder: String::new(),
        xvfb: String::new(),
        stdio: false,
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim() {
            "phos_link_port" => c.port = v.parse().map_err(|_| format!("phos_link_port isn't a port: {v}"))?,
            "phos_link_token" => c.token = v.to_string(),
            "log_path" if !v.is_empty() => c.log_path = Some(PathBuf::from(v)),
            "min_log_level" => c.log_level = crate::log::Level::parse(v),
            "dd_configuration_option" => c.primary = v == "ensure_primary",
            "adapter_name" => c.adapter = v.to_string(),
            "encoder" => c.encoder = v.to_ascii_lowercase(),
            "xvfb" => c.xvfb = v.to_string(),
            _ => {}
        }
    }
    if c.port == 0 {
        return Err("phos_link_port is missing".into());
    }
    if c.token.chars().count() < 16 {
        return Err("phos_link_token is missing or shorter than 16 characters".into());
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keys_read_the_rest_ignored() {
        let c = parse(
            "# written by phosphorus\r\nsunshine_name = x\r\nphos_link_port = 47100\r\nphos_link_token = abababababababababab\r\n\
             log_path = C:\\d\\afterglow.log\r\nmin_log_level = warning\r\ndd_configuration_option = ensure_primary\r\nupnp = off\r\n",
        )
        .unwrap();
        assert_eq!(c.port, 47100);
        assert_eq!(c.token, "abababababababababab");
        assert_eq!(c.log_path, Some(PathBuf::from("C:\\d\\afterglow.log")));
        assert_eq!(c.log_level, crate::log::Level::Warning);
        assert!(c.primary);
        assert!(c.encoder.is_empty());
    }

    #[test]
    fn no_port_or_a_short_token_refused() {
        assert!(parse("phos_link_token = abababababababababab").is_err());
        assert!(parse("phos_link_port = 1\nphos_link_token = short").is_err());
        assert!(parse("phos_link_port = x\nphos_link_token = abababababababababab").is_err());
    }
}
