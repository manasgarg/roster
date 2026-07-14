//! Host-minted CA + per-host leaf minting.
//!
//! The CA key lives at `<data>/ca` (override with `ROSTER_CA_DIR`), never on
//! the box. Per-host leaf certs are minted on demand and signed by the CA so the
//! gateway can terminate TLS for any host the box dials. See docs/gateway.md.

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

fn ca_dir() -> PathBuf {
    crate::paths::ca_dir()
}

/// Where the host keeps its system roots (Debian/Ubuntu layout).
const SYSTEM_ROOTS: &str = "/etc/ssl/certs/ca-certificates.crt";

/// Ensure `ca/bundle.crt` — the one trust file the box points every TLS stack
/// at (`SSL_CERT_FILE`, `CURL_CA_BUNDLE`, …). It must carry BOTH the system
/// roots and the roster CA: `SSL_CERT_FILE` *replaces* a client's default
/// roots, and tunnel-verdict hosts (cert-pinning clients) present real
/// certificates while terminated hosts present ours. Rebuilt whenever either
/// input is newer. Errors if the CA itself is missing — the gateway mints it.
pub fn ensure_bundle() -> Result<PathBuf, Box<dyn Error>> {
    let dir = ca_dir();
    let ca_cert = dir.join("ca.crt");
    if !ca_cert.exists() {
        return Err(format!(
            "no CA at {} — start the gateway first (roster server start creates it)",
            ca_cert.display()
        )
        .into());
    }
    let bundle = dir.join("bundle.crt");
    write_bundle(Path::new(SYSTEM_ROOTS), &ca_cert, &bundle)?;
    Ok(bundle)
}

/// Concatenate system roots + the roster CA into `bundle`, only when stale.
/// A host without system roots gets a CA-only bundle (tunneled hosts would
/// fail verification there, but nothing terminated breaks) with a warning.
fn write_bundle(system_roots: &Path, ca_cert: &Path, bundle: &Path) -> Result<(), Box<dyn Error>> {
    let mtime = |p: &Path| p.metadata().and_then(|m| m.modified()).ok();
    if let Some(built) = mtime(bundle) {
        let stale = [system_roots, ca_cert]
            .iter()
            .filter_map(|p| mtime(p))
            .any(|input| input > built);
        if !stale {
            return Ok(());
        }
    }
    let roots = match fs::read_to_string(system_roots) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "ca: no system roots at {} ({e}) — bundle will carry the roster CA only",
                system_roots.display()
            );
            String::new()
        }
    };
    let ca = fs::read_to_string(ca_cert)?;
    let newline = if roots.is_empty() || roots.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    fs::write(bundle, format!("{roots}{newline}{ca}"))?;
    Ok(())
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

        // Keep the box-facing trust bundle fresh alongside the CA itself.
        ensure_bundle()?;

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
    fn bundle_concatenates_roots_and_ca_and_rebuilds_when_stale() {
        // A sibling of temp_ca_dir(), which the leaf test removes recursively.
        let dir = std::env::temp_dir().join(format!("roster-bundle-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let roots = dir.join("roots.crt");
        let ca = dir.join("ca.crt");
        let bundle = dir.join("bundle.crt");
        fs::write(&roots, "ROOTS\n").unwrap();
        fs::write(&ca, "CA\n").unwrap();

        write_bundle(&roots, &ca, &bundle).unwrap();
        assert_eq!(fs::read_to_string(&bundle).unwrap(), "ROOTS\nCA\n");

        // Fresh: an unchanged input does not rewrite.
        let before = fs::metadata(&bundle).unwrap().modified().unwrap();
        write_bundle(&roots, &ca, &bundle).unwrap();
        assert_eq!(fs::metadata(&bundle).unwrap().modified().unwrap(), before);

        // Stale: a newer CA rebuilds the bundle.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&ca, "CA2\n").unwrap();
        write_bundle(&roots, &ca, &bundle).unwrap();
        assert_eq!(fs::read_to_string(&bundle).unwrap(), "ROOTS\nCA2\n");

        // Missing system roots: CA-only bundle, no error.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&ca, "CA3\n").unwrap();
        write_bundle(Path::new("/nonexistent/roots.crt"), &ca, &bundle).unwrap();
        assert_eq!(fs::read_to_string(&bundle).unwrap(), "CA3\n");

        let _ = fs::remove_dir_all(&dir);
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
