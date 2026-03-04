// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Sentrisense
//
//! Configuration loaded from environment variables.
//!
//! All settings have sensible defaults so only the NATS stream/consumer names
//! are strictly required for the bridge to start.

use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    // ── NATS ──────────────────────────────────────────────────────────────────
    /// NATS server URL (default: `nats://localhost:4222`)
    pub nats_url: String,
    /// JetStream stream to subscribe to (required)
    pub nats_stream: String,
    /// Durable consumer name on that stream (required)
    pub nats_consumer: String,
    /// Optional subject filter applied at subscription time.
    /// When set, only messages published on this subject are processed.
    pub nats_subject_filter: Option<String>,

    // ── IEC-104 server ────────────────────────────────────────────────────────
    /// Network address for the IEC-104 server to bind on (default: `0.0.0.0`)
    pub iec104_bind_addr: String,
    /// TCP port for the IEC-104 server (default: `2404`)
    pub iec104_port: u16,
    /// Default Common Address (ASDU address) when not supplied in the JSON
    /// message (default: `1`)
    pub iec104_default_ca: u16,

    // ── Observability ─────────────────────────────────────────────────────────
    /// TCP port for the Prometheus metrics HTTP endpoint (default: `9091`).
    ///
    /// Set via the `METRICS_PORT` environment variable.
    pub metrics_port: u16,

    // ── TLS / IEC 62351-3 ────────────────────────────────────────────────────
    /// Enable Mutual TLS (IEC 62351-3 compliant transport security).
    ///
    /// When `true` the bridge starts a TLS listener on [`Self::tls_port`] and
    /// requires all three certificate paths to be set.  The `lib60870` server
    /// is automatically re-bound to loopback so that only the TLS listener
    /// accepts external connections.
    ///
    /// Set via `TLS_ENABLED=true` (default: `false`).
    pub tls_enabled: bool,
    /// Path to the server's PEM-encoded certificate chain.
    ///
    /// Required when `tls_enabled = true`.  Set via `TLS_CERT_PATH`.
    pub tls_cert_path: Option<String>,
    /// Path to the server's PEM-encoded private key.
    ///
    /// Required when `tls_enabled = true`.  Set via `TLS_KEY_PATH`.
    pub tls_key_path: Option<String>,
    /// Path to the PEM-encoded Root CA certificate used to verify **client**
    /// certificates (Mutual TLS).
    ///
    /// Required when `tls_enabled = true`.  Set via `TLS_CA_CERT_PATH`.
    pub tls_ca_cert_path: Option<String>,
    /// TCP port for the external TLS listener (default: `19998`).
    ///
    /// Port 19998 is the IANA-registered port for IEC 62351-3 secured IEC-104.
    /// Set via `TLS_PORT`.
    pub tls_port: u16,
}

impl Config {
    /// Build a [`Config`] by reading environment variables.
    ///
    /// # Errors
    /// Returns an error if `NATS_STREAM` or `NATS_CONSUMER` are not set.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|key| env::var(key).ok())
    }

    /// Build a [`Config`] from an arbitrary key–value lookup.
    ///
    /// The lookup closure mirrors `std::env::var`: return `Some(value)` when
    /// the key is present, `None` otherwise.
    ///
    /// This variant is the testable entry-point; production code should call
    /// [`Self::from_env`] which delegates here with the real environment.
    ///
    /// # Errors
    /// Returns an error if `NATS_STREAM` or `NATS_CONSUMER` are absent, or if
    /// `IEC104_PORT` / `IEC104_CA` cannot be parsed as `u16`.
    pub fn from_lookup<F>(get: F) -> anyhow::Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let nats_stream =
            get("NATS_STREAM").ok_or_else(|| anyhow::anyhow!("NATS_STREAM must be set"))?;
        let nats_consumer =
            get("NATS_CONSUMER").ok_or_else(|| anyhow::anyhow!("NATS_CONSUMER must be set"))?;

        let nats_url = get("NATS_URL").unwrap_or_else(|| "nats://localhost:4222".into());
        let nats_subject_filter = get("NATS_SUBJECT_FILTER");

        let iec104_bind_addr = get("IEC104_BIND_ADDR").unwrap_or_else(|| "0.0.0.0".into());
        let iec104_port = get("IEC104_PORT")
            .unwrap_or_else(|| "2404".into())
            .parse::<u16>()
            .map_err(|_| anyhow::anyhow!("IEC104_PORT must be a valid u16"))?;
        let iec104_default_ca = get("IEC104_CA")
            .unwrap_or_else(|| "1".into())
            .parse::<u16>()
            .map_err(|_| anyhow::anyhow!("IEC104_CA must be a valid u16"))?;

        let metrics_port = get("METRICS_PORT")
            .unwrap_or_else(|| "9091".into())
            .parse::<u16>()
            .map_err(|_| anyhow::anyhow!("METRICS_PORT must be a valid u16"))?;

        // ── TLS / IEC 62351-3 ────────────────────────────────────────────────
        let tls_enabled = get("TLS_ENABLED")
            .map(|v| v.to_lowercase())
            .map(|v| v == "true" || v == "1" || v == "yes")
            .unwrap_or(false);

        let tls_cert_path = get("TLS_CERT_PATH");
        let tls_key_path = get("TLS_KEY_PATH");
        let tls_ca_cert_path = get("TLS_CA_CERT_PATH");

        let tls_port = get("TLS_PORT")
            .unwrap_or_else(|| "19998".into())
            .parse::<u16>()
            .map_err(|_| anyhow::anyhow!("TLS_PORT must be a valid u16"))?;

        if tls_enabled {
            if tls_cert_path.is_none() {
                anyhow::bail!("TLS_CERT_PATH must be set when TLS_ENABLED=true");
            }
            if tls_key_path.is_none() {
                anyhow::bail!("TLS_KEY_PATH must be set when TLS_ENABLED=true");
            }
            if tls_ca_cert_path.is_none() {
                anyhow::bail!("TLS_CA_CERT_PATH must be set when TLS_ENABLED=true");
            }
        }

        Ok(Self {
            nats_url,
            nats_stream,
            nats_consumer,
            nats_subject_filter,
            iec104_bind_addr,
            iec104_port,
            iec104_default_ca,
            metrics_port,
            tls_enabled,
            tls_cert_path,
            tls_key_path,
            tls_ca_cert_path,
            tls_port,
        })
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::Config;
    use std::collections::HashMap;

    /// Build a Config from a static key–value map.
    fn from_map(map: &HashMap<&str, &str>) -> anyhow::Result<Config> {
        Config::from_lookup(|key| map.get(key).map(|v| v.to_string()))
    }

    fn required() -> HashMap<&'static str, &'static str> {
        let mut m = HashMap::new();
        m.insert("NATS_STREAM", "sensors");
        m.insert("NATS_CONSUMER", "bridge");
        m
    }

    // ── required fields ───────────────────────────────────────────────────────

    #[test]
    fn missing_nats_stream_is_error() {
        let mut map = required();
        map.remove("NATS_STREAM");
        assert!(from_map(&map).is_err());
    }

    #[test]
    fn missing_nats_consumer_is_error() {
        let mut map = required();
        map.remove("NATS_CONSUMER");
        assert!(from_map(&map).is_err());
    }

    // ── defaults ──────────────────────────────────────────────────────────────

    #[test]
    fn default_nats_url() {
        let cfg = from_map(&required()).unwrap();
        assert_eq!(cfg.nats_url, "nats://localhost:4222");
    }

    #[test]
    fn default_iec104_port() {
        let cfg = from_map(&required()).unwrap();
        assert_eq!(cfg.iec104_port, 2404);
    }

    #[test]
    fn default_iec104_ca() {
        let cfg = from_map(&required()).unwrap();
        assert_eq!(cfg.iec104_default_ca, 1);
    }

    #[test]
    fn default_bind_addr() {
        let cfg = from_map(&required()).unwrap();
        assert_eq!(cfg.iec104_bind_addr, "0.0.0.0");
    }

    #[test]
    fn default_subject_filter_is_none() {
        let cfg = from_map(&required()).unwrap();
        assert_eq!(cfg.nats_subject_filter, None);
    }

    // ── overrides ─────────────────────────────────────────────────────────────

    #[test]
    fn custom_nats_url() {
        let mut map = required();
        map.insert("NATS_URL", "nats://nats.example.com:4222");
        let cfg = from_map(&map).unwrap();
        assert_eq!(cfg.nats_url, "nats://nats.example.com:4222");
    }

    #[test]
    fn custom_iec104_port() {
        let mut map = required();
        map.insert("IEC104_PORT", "12345");
        let cfg = from_map(&map).unwrap();
        assert_eq!(cfg.iec104_port, 12345);
    }

    #[test]
    fn custom_iec104_ca() {
        let mut map = required();
        map.insert("IEC104_CA", "42");
        let cfg = from_map(&map).unwrap();
        assert_eq!(cfg.iec104_default_ca, 42);
    }

    #[test]
    fn custom_subject_filter() {
        let mut map = required();
        map.insert("NATS_SUBJECT_FILTER", "plant.a.>");
        let cfg = from_map(&map).unwrap();
        assert_eq!(cfg.nats_subject_filter, Some("plant.a.>".into()));
    }

    #[test]
    fn custom_bind_addr() {
        let mut map = required();
        map.insert("IEC104_BIND_ADDR", "127.0.0.1");
        let cfg = from_map(&map).unwrap();
        assert_eq!(cfg.iec104_bind_addr, "127.0.0.1");
    }

    // ── parse errors ──────────────────────────────────────────────────────────

    #[test]
    fn invalid_port_is_error() {
        let mut map = required();
        map.insert("IEC104_PORT", "not_a_number");
        assert!(from_map(&map).is_err());
    }

    #[test]
    fn port_out_of_u16_range_is_error() {
        let mut map = required();
        map.insert("IEC104_PORT", "99999");
        assert!(from_map(&map).is_err());
    }

    #[test]
    fn invalid_ca_is_error() {
        let mut map = required();
        map.insert("IEC104_CA", "not_a_number");
        assert!(from_map(&map).is_err());
    }

    #[test]
    fn stream_and_consumer_are_stored() {
        let cfg = from_map(&required()).unwrap();
        assert_eq!(cfg.nats_stream, "sensors");
        assert_eq!(cfg.nats_consumer, "bridge");
    }

    // ── TLS / IEC 62351-3 ─────────────────────────────────────────────────────

    #[test]
    fn tls_disabled_by_default() {
        let cfg = from_map(&required()).unwrap();
        assert!(!cfg.tls_enabled);
        assert_eq!(cfg.tls_port, 19998);
        assert!(cfg.tls_cert_path.is_none());
        assert!(cfg.tls_key_path.is_none());
        assert!(cfg.tls_ca_cert_path.is_none());
    }

    #[test]
    fn tls_enabled_requires_cert_path() {
        let mut map = required();
        map.insert("TLS_ENABLED", "true");
        map.insert("TLS_KEY_PATH", "/key.pem");
        map.insert("TLS_CA_CERT_PATH", "/ca.pem");
        // TLS_CERT_PATH missing
        assert!(from_map(&map).is_err());
    }

    #[test]
    fn tls_enabled_requires_key_path() {
        let mut map = required();
        map.insert("TLS_ENABLED", "true");
        map.insert("TLS_CERT_PATH", "/cert.pem");
        map.insert("TLS_CA_CERT_PATH", "/ca.pem");
        // TLS_KEY_PATH missing
        assert!(from_map(&map).is_err());
    }

    #[test]
    fn tls_enabled_requires_ca_cert_path() {
        let mut map = required();
        map.insert("TLS_ENABLED", "true");
        map.insert("TLS_CERT_PATH", "/cert.pem");
        map.insert("TLS_KEY_PATH", "/key.pem");
        // TLS_CA_CERT_PATH missing
        assert!(from_map(&map).is_err());
    }

    #[test]
    fn tls_enabled_with_all_paths_ok() {
        let mut map = required();
        map.insert("TLS_ENABLED", "true");
        map.insert("TLS_CERT_PATH", "/cert.pem");
        map.insert("TLS_KEY_PATH", "/key.pem");
        map.insert("TLS_CA_CERT_PATH", "/ca.pem");
        let cfg = from_map(&map).unwrap();
        assert!(cfg.tls_enabled);
        assert_eq!(cfg.tls_cert_path, Some("/cert.pem".into()));
        assert_eq!(cfg.tls_key_path, Some("/key.pem".into()));
        assert_eq!(cfg.tls_ca_cert_path, Some("/ca.pem".into()));
    }

    #[test]
    fn tls_enabled_truthy_values() {
        for val in &["true", "1", "yes", "True", "TRUE", "YES"] {
            let mut map = required();
            map.insert("TLS_ENABLED", val);
            map.insert("TLS_CERT_PATH", "/c.pem");
            map.insert("TLS_KEY_PATH", "/k.pem");
            map.insert("TLS_CA_CERT_PATH", "/ca.pem");
            let cfg = from_map(&map).unwrap();
            assert!(
                cfg.tls_enabled,
                "Expected tls_enabled=true for TLS_ENABLED={val}"
            );
        }
    }

    #[test]
    fn custom_tls_port() {
        let mut map = required();
        map.insert("TLS_PORT", "8883");
        let cfg = from_map(&map).unwrap();
        assert_eq!(cfg.tls_port, 8883);
    }
}
