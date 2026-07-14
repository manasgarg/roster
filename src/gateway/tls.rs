//! TLS termination: a rustls server that mints a leaf cert per SNI hostname
//! from our CA, so the gateway can decrypt whatever host the box dials.
//! See docs/gateway.md.

use crate::gateway::ca::Ca;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::ServerConfig;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_rustls::TlsAcceptor;

struct SniResolver {
    ca: Arc<Ca>,
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl std::fmt::Debug for SniResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SniResolver")
    }
}

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, hello: ClientHello) -> Option<Arc<CertifiedKey>> {
        let name = hello.server_name()?.to_string();
        {
            let cache = self.cache.lock().unwrap();
            if let Some(ck) = cache.get(&name) {
                return Some(ck.clone());
            }
        }
        let (cert_der, key_der) = self.ca.mint_leaf_der(&name).ok()?;
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));
        let signing_key = rustls::crypto::ring::sign::any_supported_type(&key).ok()?;
        let ck = Arc::new(CertifiedKey::new(
            vec![CertificateDer::from(cert_der)],
            signing_key,
        ));
        self.cache.lock().unwrap().insert(name, ck.clone());
        Some(ck)
    }
}

pub fn acceptor(ca: Arc<Ca>) -> TlsAcceptor {
    let resolver = Arc::new(SniResolver {
        ca,
        cache: Mutex::new(HashMap::new()),
    });
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    TlsAcceptor::from(Arc::new(config))
}
