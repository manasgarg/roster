//! Host-minted CA + per-host leaf minting (the Rust port of `src/ca.ts`).
//!
//! The CA key lives at `~/.roster/ca` (override with `ROSTER_CA_DIR`), never on
//! the box. Per-host leaf certs are minted on demand and signed by the CA so the
//! gateway can terminate TLS for any host the box dials. `rcgen` replaces the
//! `openssl` shell-outs the TS version used. See docs/rust-port.md (P0).

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use std::error::Error;
use std::fs;
use std::path::PathBuf;

fn ca_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ROSTER_CA_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".roster").join("ca")
}

/// The CA, loaded (or generated) from disk. Holds the PEMs so leaves can be
/// minted without re-reading files each time.
pub struct Ca {
    key_pem: String,
    cert_pem: String,
}

impl Ca {
    /// Ensure the CA exists on disk (generate if absent) and load it.
    pub fn ensure() -> Result<Ca, Box<dyn Error>> {
        let dir = ca_dir();
        fs::create_dir_all(&dir)?;
        let key_path = dir.join("ca.key");
        let cert_path = dir.join("ca.crt");

        if !key_path.exists() || !cert_path.exists() {
            let key = KeyPair::generate()?;
            let mut params = CertificateParams::new(Vec::<String>::new())?;
            params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            params
                .distinguished_name
                .push(DnType::CommonName, "Roster Box CA");
            let cert = params.self_signed(&key)?;
            fs::write(&key_path, key.serialize_pem())?;
            fs::write(&cert_path, cert.pem())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))?;
            }
        }

        Ok(Ca {
            key_pem: fs::read_to_string(&key_path)?,
            cert_pem: fs::read_to_string(&cert_path)?,
        })
    }

    /// The public CA cert (PEM) — the only part the box is given, as a trust anchor.
    #[allow(dead_code)] // used by tests; the proxy path uses mint_leaf_der
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    fn issuer(&self) -> Result<Issuer<'static, KeyPair>, Box<dyn Error>> {
        let key = KeyPair::from_pem(&self.key_pem)?;
        Ok(Issuer::from_ca_cert_pem(&self.cert_pem, key)?)
    }

    /// Mint a leaf cert for `host` (SAN=host), signed by the CA. Returns
    /// `(leaf_key_pem, chain_pem)` where the chain is leaf + CA.
    #[allow(dead_code)] // used by tests; the proxy path uses mint_leaf_der
    pub fn mint_leaf(&self, host: &str) -> Result<(String, String), Box<dyn Error>> {
        let issuer = self.issuer()?;
        let leaf_key = KeyPair::generate()?;
        let params = CertificateParams::new(vec![host.to_string()])?;
        let leaf = params.signed_by(&leaf_key, &issuer)?;
        let chain = format!("{}{}", leaf.pem(), self.cert_pem);
        Ok((leaf_key.serialize_pem(), chain))
    }

    /// Mint a leaf for `host` as DER for rustls: `(cert_der, key_pkcs8_der)`.
    /// The box trusts our CA as a root, so presenting the leaf alone suffices.
    pub fn mint_leaf_der(&self, host: &str) -> Result<(Vec<u8>, Vec<u8>), Box<dyn Error>> {
        let issuer = self.issuer()?;
        let leaf_key = KeyPair::generate()?;
        let params = CertificateParams::new(vec![host.to_string()])?;
        let leaf = params.signed_by(&leaf_key, &issuer)?;
        Ok((leaf.der().as_ref().to_vec(), leaf_key.serialize_der()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x509_parser::prelude::*;

    fn temp_ca_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("roster-ca-test-{}", std::process::id()));
        dir
    }

    #[test]
    fn mints_a_leaf_with_correct_san_signed_by_the_ca() {
        let dir = temp_ca_dir();
        std::env::set_var("ROSTER_CA_DIR", &dir);

        let ca = Ca::ensure().expect("ensure CA");
        let (key_pem, chain_pem) = ca.mint_leaf("chatgpt.com").expect("mint leaf");

        // A private key and a two-cert chain (leaf + CA) came back.
        assert!(key_pem.contains("PRIVATE KEY"));
        assert_eq!(chain_pem.matches("BEGIN CERTIFICATE").count(), 2);

        // The leaf carries SAN=chatgpt.com and is issued by "Roster Box CA".
        let leaf_der = pem::parse_x509_pem(chain_pem.as_bytes()).unwrap().1;
        let (_, leaf) = parse_x509_certificate(&leaf_der.contents).unwrap();
        assert!(leaf.issuer().to_string().contains("Roster Box CA"));
        let san = leaf
            .get_extension_unique(&oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME)
            .unwrap()
            .expect("SAN present");
        if let ParsedExtension::SubjectAlternativeName(names) = san.parsed_extension() {
            let dns: Vec<_> = names
                .general_names
                .iter()
                .filter_map(|n| match n {
                    GeneralName::DNSName(d) => Some(*d),
                    _ => None,
                })
                .collect();
            assert_eq!(dns, vec!["chatgpt.com"]);
        } else {
            panic!("SAN extension did not parse");
        }

        // Idempotent: a second ensure loads the same CA.
        let ca2 = Ca::ensure().expect("re-ensure CA");
        assert_eq!(ca.cert_pem(), ca2.cert_pem());

        let _ = fs::remove_dir_all(&dir);
    }
}
