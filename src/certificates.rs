use crate::model::Certificate;
use anyhow::{Result, ensure};
use openssl::asn1::Asn1Time;
use std::{cmp::Ordering, net::IpAddr};

impl Certificate {
    pub fn parse(cert: &[u8], key: &[u8]) -> Result<Self> {
        let mut chain = openssl::x509::X509::stack_from_pem(cert)?;
        ensure!(!chain.is_empty(), "empty certificate chain");
        let leaf = chain.remove(0);
        let key = openssl::pkey::PKey::private_key_from_pem(key)?;
        ensure!(
            leaf.public_key()?.public_eq(&key),
            "certificate and private key do not match"
        );
        let cert = Self { leaf, chain, key };
        cert.valid_time()?;
        Ok(cert)
    }
    pub fn valid_time(&self) -> Result<()> {
        let now = Asn1Time::days_from_now(0)?;
        ensure!(
            self.leaf.not_before().compare(&now)? != Ordering::Greater,
            "certificate is not yet valid"
        );
        ensure!(
            self.leaf.not_after().compare(&now)? == Ordering::Greater,
            "certificate has expired"
        );
        Ok(())
    }
    pub fn expires_at(&self) -> Result<i64> {
        let diff = Asn1Time::from_unix(0)?.diff(self.leaf.not_after())?;
        Ok(i64::from(diff.days) * 86400 + i64::from(diff.secs))
    }
    pub fn matches_name(&self, name: &str) -> bool {
        if name.is_empty() || name == "_" {
            return true;
        }
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        let ip = name.parse::<IpAddr>().ok();
        if let Some(san) = self.leaf.subject_alt_names() {
            let relevant = san
                .iter()
                .any(|entry| entry.dnsname().is_some() || entry.ipaddress().is_some());
            if relevant {
                return san.iter().any(|entry| {
                    if let Some(ip) = ip {
                        return entry.ipaddress().is_some_and(|bytes| match ip {
                            IpAddr::V4(ip) => bytes == ip.octets(),
                            IpAddr::V6(ip) => bytes == ip.octets(),
                        });
                    }
                    entry.dnsname().is_some_and(|dns| dns_match(dns, &name))
                });
            }
        }
        ip.is_none()
            && self
                .leaf
                .subject_name()
                .entries_by_nid(openssl::nid::Nid::COMMONNAME)
                .any(|entry| {
                    entry
                        .data()
                        .to_string()
                        .is_ok_and(|cn| dns_match(&cn, &name))
                })
    }
    pub fn validate_name(&self, name: &str) -> Result<()> {
        ensure!(
            self.matches_name(name),
            "certificate does not cover configured host {name}"
        );
        Ok(())
    }
    pub fn diagnostic(&self, name: &str) -> serde_json::Value {
        let expires = self.expires_at().ok();
        let now = k8s_openapi::chrono::Utc::now().timestamp();
        serde_json::json!({"not_before":self.leaf.not_before().to_string(),"not_after":self.leaf.not_after().to_string(),"expires_at":expires,"valid":self.valid_time().is_ok() && self.matches_name(name),"hostname_matches":self.matches_name(name),"expiring_soon":expires.is_some_and(|t| t-now <= 14*86400)})
    }
}
fn dns_match(pattern: &str, name: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    pattern == name
        || (!name.starts_with("*.")
            && pattern.strip_prefix("*.").is_some_and(|suffix| {
                name.strip_suffix(suffix)
                    .and_then(|p| p.strip_suffix('.'))
                    .is_some_and(|p| !p.is_empty() && !p.contains('.'))
            }))
}
