use std::ffi::CString;

pub(crate) struct HarmonyConfiguration {
    pub(crate) port: u16,
    pub(crate) cache_directory: CString,
    pub(crate) allowed_hosts: CString,
}

impl HarmonyConfiguration {
    pub(crate) fn parse(
        port: u32,
        cache_directory: String,
        allowed_hosts: Vec<String>,
    ) -> Result<Self, &'static str> {
        let port = u16::try_from(port).map_err(|_| "invalid proxy port")?;
        if cache_directory.trim().is_empty() {
            return Err("invalid cache directory");
        }
        let cache_directory =
            CString::new(cache_directory).map_err(|_| "invalid cache directory")?;
        if allowed_hosts.is_empty()
            || allowed_hosts
                .iter()
                .any(|host| host.trim().is_empty() || host.contains(','))
        {
            return Err("invalid allowed host");
        }
        let allowed_hosts = allowed_hosts
            .into_iter()
            .map(|host| host.trim().to_owned())
            .collect::<Vec<_>>()
            .join(",");
        let allowed_hosts = CString::new(allowed_hosts).map_err(|_| "invalid allowed host")?;
        Ok(Self {
            port,
            cache_directory,
            allowed_hosts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_dynamic_port_and_normalizes_host_whitespace() {
        let config = HarmonyConfiguration::parse(
            0,
            "/tmp/cache".to_owned(),
            vec![" example.com ".to_owned(), "cdn.example.com".to_owned()],
        )
        .unwrap();
        assert_eq!(config.port, 0);
        assert_eq!(config.cache_directory.to_str().unwrap(), "/tmp/cache");
        assert_eq!(
            config.allowed_hosts.to_str().unwrap(),
            "example.com,cdn.example.com"
        );
    }

    #[test]
    fn rejects_out_of_range_port_and_empty_required_values() {
        assert!(HarmonyConfiguration::parse(
            u16::MAX as u32 + 1,
            "/tmp/cache".to_owned(),
            vec!["example.com".to_owned()]
        )
        .is_err());
        assert!(
            HarmonyConfiguration::parse(0, "  ".to_owned(), vec!["example.com".to_owned()])
                .is_err()
        );
        assert!(HarmonyConfiguration::parse(0, "/tmp/cache".to_owned(), Vec::new()).is_err());
    }

    #[test]
    fn rejects_delimiter_and_nul_injection() {
        for host in ["example.com,internal", "bad\0host", "   "] {
            assert!(
                HarmonyConfiguration::parse(0, "/tmp/cache".to_owned(), vec![host.to_owned()])
                    .is_err()
            );
        }
        assert!(HarmonyConfiguration::parse(
            0,
            "/tmp/ca\0che".to_owned(),
            vec!["example.com".to_owned()]
        )
        .is_err());
    }
}
