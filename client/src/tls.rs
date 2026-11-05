//! QUIC-клиент: TLS с «пришпиленным» отпечатком сертификата сервера (самоподписанный, без CA).

use anyhow::{ensure, Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug)]
struct Pinned {
    fp: Vec<u8>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if Sha256::digest(end_entity.as_ref()).as_slice() == self.fp.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("сертификат сервера не совпадает с ожидаемым".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

pub fn transport_config() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.max_concurrent_uni_streams(4096u32.into());
    t.max_concurrent_bidi_streams(4u32.into());
    t.keep_alive_interval(Some(Duration::from_secs(4)));
    t.max_idle_timeout(Some(Duration::from_secs(20).try_into().unwrap()));
    t.stream_receive_window((8u32 << 20).into());
    t.receive_window((32u32 << 20).into());
    // См. сервер: маленькое окно отправки — затор копится в нашей очереди, где мы умеем выкидывать кадры.
    t.send_window(600_000);
    t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    t
}

pub fn client_config(fingerprint_hex: &str) -> Result<quinn::ClientConfig> {
    let fp = hex::decode(fingerprint_hex.trim().replace(':', "")).context("отпечаток сервера — не hex")?;
    ensure!(fp.len() == 32, "отпечаток сервера должен быть SHA-256 (64 hex-символа)");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned { fp, provider }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![proto::ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)?;
    let mut cc = quinn::ClientConfig::new(Arc::new(crypto));
    cc.transport_config(Arc::new(transport_config()));
    Ok(cc)
}

/// Отпечаток из файла сертификата (для самопроверки рядом с локальным сервером).
pub fn fingerprint_of_file(path: &str) -> Result<String> {
    let der = std::fs::read(path).with_context(|| format!("не читается {path}"))?;
    Ok(proto::fingerprint_hex(&Sha256::digest(&der)))
}
