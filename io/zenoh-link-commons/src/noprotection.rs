use std::sync::Arc;

use bytes::BytesMut;
use quinn_proto::{
    crypto::{
        self,
        rustls::{QuicClientConfig, QuicServerConfig},
        CryptoError,
    },
    transport_parameters, ConnectionId, Side, TransportError,
};

struct PlainTextSession(Box<dyn crypto::Session>);

impl PlainTextSession {
    fn wrap_packet_keys(
        keys: crypto::KeyPair<Box<dyn crypto::PacketKey>>,
    ) -> crypto::KeyPair<Box<dyn crypto::PacketKey>> {
        crypto::KeyPair {
            local: Box::new(NoProtectionPacketKey(keys.local)),
            remote: Box::new(NoProtectionPacketKey(keys.remote)),
        }
    }
}

struct NoProtectionPacketKey(Box<dyn crypto::PacketKey>);

impl crypto::PacketKey for NoProtectionPacketKey {
    fn encrypt(&self, _packet: u64, buf: &mut [u8], header_len: usize) {
        let (_header, payload_tag) = buf.split_at_mut(header_len);
        let (_payload, tag) = payload_tag.split_at_mut(payload_tag.len() - self.0.tag_len());
        // There is no AEAD encryption, therefore fill authentication tag with '*'
        tag.fill(42);
    }

    fn decrypt(
        &self,
        _packet: u64,
        _header: &[u8],
        payload: &mut BytesMut,
    ) -> Result<(), CryptoError> {
        let plain_len = payload.len() - self.0.tag_len();
        payload.truncate(plain_len);
        Ok(())
    }

    fn tag_len(&self) -> usize {
        self.0.tag_len()
    }

    fn confidentiality_limit(&self) -> u64 {
        self.0.confidentiality_limit()
    }

    fn integrity_limit(&self) -> u64 {
        self.0.integrity_limit()
    }
}

pub struct PlainTextClientConfig {
    inner: Arc<QuicClientConfig>,
}

impl PlainTextClientConfig {
    pub fn new(config: Arc<QuicClientConfig>) -> Self {
        Self { inner: config }
    }
}

pub struct PlainTextServerConfig {
    inner: Arc<QuicServerConfig>,
}

impl PlainTextServerConfig {
    pub fn new(config: Arc<QuicServerConfig>) -> Self {
        Self { inner: config }
    }
}

// forward all calls to inner except those related to packet encryption/decryption
impl crypto::Session for PlainTextSession {
    fn initial_keys(&self, dst_cid: &ConnectionId, side: Side) -> crypto::Keys {
        self.0.initial_keys(dst_cid, side)
    }

    fn handshake_data(&self) -> Option<Box<dyn std::any::Any>> {
        self.0.handshake_data()
    }

    fn peer_identity(&self) -> Option<Box<dyn std::any::Any>> {
        self.0.peer_identity()
    }

    fn early_crypto(&self) -> Option<(Box<dyn crypto::HeaderKey>, Box<dyn crypto::PacketKey>)> {
        let (hkey, pkey) = self.0.early_crypto()?;

        // use wrapper type to disable packet encryption/decryption
        Some((hkey, Box::new(NoProtectionPacketKey(pkey))))
    }

    fn early_data_accepted(&self) -> Option<bool> {
        self.0.early_data_accepted()
    }

    fn is_handshaking(&self) -> bool {
        self.0.is_handshaking()
    }

    fn read_handshake(&mut self, buf: &[u8]) -> Result<bool, TransportError> {
        self.0.read_handshake(buf)
    }

    fn transport_parameters(
        &self,
    ) -> Result<Option<transport_parameters::TransportParameters>, TransportError> {
        self.0.transport_parameters()
    }

    fn write_handshake(&mut self, buf: &mut Vec<u8>) -> Option<crypto::Keys> {
        let keys = self.0.write_handshake(buf)?;

        Some(crypto::Keys {
            header: keys.header,
            packet: Self::wrap_packet_keys(keys.packet),
        })
    }

    fn next_1rtt_keys(&mut self) -> Option<crypto::KeyPair<Box<dyn crypto::PacketKey>>> {
        let keys = self.0.next_1rtt_keys()?;
        Some(Self::wrap_packet_keys(keys))
    }

    fn is_valid_retry(&self, orig_dst_cid: &ConnectionId, header: &[u8], payload: &[u8]) -> bool {
        self.0.is_valid_retry(orig_dst_cid, header, payload)
    }

    fn export_keying_material(
        &self,
        output: &mut [u8],
        label: &[u8],
        context: &[u8],
    ) -> Result<(), crypto::ExportKeyingMaterialError> {
        self.0.export_keying_material(output, label, context)
    }
}

impl crypto::ClientConfig for PlainTextClientConfig {
    fn start_session(
        self: std::sync::Arc<Self>,
        version: u32,
        server_name: &str,
        params: &transport_parameters::TransportParameters,
    ) -> Result<Box<dyn crypto::Session>, quinn::ConnectError> {
        let tls = self
            .inner
            .clone()
            .start_session(version, server_name, params)?;

        Ok(Box::new(PlainTextSession(tls)))
    }
}

impl crypto::ServerConfig for PlainTextServerConfig {
    fn initial_keys(
        &self,
        version: u32,
        dst_cid: &ConnectionId,
    ) -> Result<crypto::Keys, crypto::UnsupportedVersion> {
        self.inner.initial_keys(version, dst_cid)
    }

    fn retry_tag(&self, version: u32, orig_dst_cid: &ConnectionId, packet: &[u8]) -> [u8; 16] {
        self.inner.retry_tag(version, orig_dst_cid, packet)
    }

    fn start_session(
        self: Arc<Self>,
        version: u32,
        params: &transport_parameters::TransportParameters,
    ) -> Box<dyn crypto::Session> {
        Box::new(PlainTextSession(
            self.inner.clone().start_session(version, params),
        ))
    }
}
