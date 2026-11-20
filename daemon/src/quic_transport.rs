//! QUIC transport and authenticated pairing. No application traffic is permitted
//! before `authenticate` succeeds. TLS encrypts the connection; CPace binds a new
//! client's signing identity to the actual TLS server certificate and exporter.
use crate::cpace;
use crate::protocol::MEDIA_HEADER_LEN;
use anyhow::{ensure, Context, Result};
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use ring::{digest, signature};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub const ALPN: &[u8] = b"screx-quic-1";
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-screx-quic-pair-v1";
pub const MAX_FRAME: usize = 65536;
pub const CHUNK_PAYLOAD: usize = 1000;
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(120);
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(8);

const HELLO: u8 = 0xe0;
const CHALLENGE: u8 = 0xe1;
const PROOF: u8 = 0xe2;
const PAIR: u8 = 0xe3;
const PAIRED: u8 = 0xe4;
const READY: u8 = 0xe5;
/// Sent instead of READY when another device holds the session.
const BUSY: u8 = 0xe6;

#[derive(Serialize, Deserialize)]
pub struct Identity {
    pub id: [u8; 16],
    pub certificate: Vec<u8>,
    private_key: Vec<u8>,
}
impl Identity {
    pub fn load(directory: &Path) -> Result<Self> {
        let path = directory.join("quic-identity-v1.json");
        if path.exists() {
            // Never silently replace an unreadable identity (it would invalidate pins).
            return Ok(serde_json::from_slice(&fs::read(path)?)?);
        }
        let cert = rcgen::generate_simple_self_signed(vec!["screx.local".into()])?;
        let identity = Self {
            id: cpace::random(),
            certificate: cert.cert.der().to_vec(),
            private_key: cert.key_pair.serialize_der(),
        };
        write_private_json(&path, &identity)?;
        Ok(identity)
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        digest::digest(&digest::SHA256, &self.certificate)
            .as_ref()
            .try_into()
            .unwrap()
    }

    pub fn endpoint(&self, address: std::net::SocketAddr) -> Result<Endpoint> {
        let mut tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(self.certificate.clone())],
                PrivatePkcs8KeyDer::from(self.private_key.clone()).into(),
            )?;
        tls.alpn_protocols = vec![ALPN.to_vec()];
        // Never accept replayable early input.
        tls.max_early_data_size = 0;
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
        let mut server = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        let mut transport = quinn::TransportConfig::default();
        transport.max_idle_timeout(Some(Duration::from_secs(15).try_into()?));
        transport.keep_alive_interval(Some(Duration::from_secs(2)));
        // Network.framework's multiplex group reserves stream 0 before
        // extracting control/input flows. Allow that placeholder as well.
        transport.max_concurrent_bidi_streams(3u32.into());
        transport.max_concurrent_uni_streams(0u32.into());
        transport.datagram_receive_buffer_size(Some(256 * 1024));
        // Keep the datagram send backlog small: obsolete video must not accumulate.
        transport.datagram_send_buffer_size(64 * 1024);
        // BBR tolerates random cellular packet loss without halving cwnd and avoids
        // filling bottleneck queues (bufferbloat), while saturating LAN links cleanly.
        transport.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
        server.transport_config(Arc::new(transport));
        let socket = socket2::Socket::new(
            socket2::Domain::for_address(address),
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        if address.is_ipv6() {
            socket.set_only_v6(false)?;
        }
        socket.bind(&address.into())?;
        socket.set_nonblocking(true)?;
        Ok(Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(server),
            socket.into(),
            Arc::new(quinn::TokioRuntime),
        )?)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairedClient {
    pub public_key: [u8; 32],
    pub name: String,
    pub paired_at: u64,
}
#[derive(Default, Serialize, Deserialize)]
pub struct Pairings {
    pub clients: HashMap<String, PairedClient>,
    #[serde(skip)]
    path: PathBuf,
}
impl Pairings {
    pub fn load(directory: &Path) -> Result<Self> {
        let path = directory.join("quic-pairings-v1.json");
        let mut result: Self = if path.exists() {
            serde_json::from_slice(&fs::read(&path)?)?
        } else {
            Self::default()
        };
        result.path = path;
        Ok(result)
    }
    pub fn save(&self) -> Result<()> {
        write_private_json(&self.path, self)
    }
}

fn write_private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    fs::create_dir_all(path.parent().context("identity directory")?)?;
    let temporary = path.with_extension("tmp");
    // A temporary file left by an interrupted write is replaced, not fatal.
    let _ = fs::remove_file(&temporary);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .context("create private identity file")?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub async fn write_frame(stream: &mut SendStream, body: &[u8]) -> Result<()> {
    ensure!(
        !body.is_empty() && body.len() <= MAX_FRAME,
        "invalid frame length"
    );
    tokio::time::timeout(AUTH_TIMEOUT, async {
        stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
        stream.write_all(body).await
    })
    .await
    .context("QUIC frame write timed out")??;
    Ok(())
}
pub async fn read_frame(stream: &mut RecvStream) -> Result<Vec<u8>> {
    let mut header = [0; 4];
    stream.read_exact(&mut header).await?;
    let length = u32::from_be_bytes(header) as usize;
    ensure!(
        (1..=MAX_FRAME).contains(&length),
        "invalid frame length {length}"
    );
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;
    Ok(body)
}

pub fn exporter(connection: &Connection) -> Result<[u8; 32]> {
    let mut output = [0; 32];
    connection
        .export_keying_material(&mut output, EXPORTER_LABEL, b"")
        .map_err(|_| anyhow::anyhow!("TLS exporter unavailable"))?;
    Ok(output)
}

/// All identities and the current TLS channel are bound into CPace's context.
pub fn binding(
    client_id: &[u8],
    server_id: &[u8],
    client_key: &[u8],
    certificate_hash: &[u8],
    exporter: &[u8],
) -> Vec<u8> {
    cpace::lv(&[
        b"screx-quic-pair-v1",
        client_id,
        server_id,
        client_key,
        certificate_hash,
        exporter,
    ])
}

pub struct Authenticated {
    pub device_id: String,
    pub name: String,
}

/// First bidirectional stream is pairing, then control. No TLS client
/// certificate is required: a pinned Ed25519 identity proves client ownership on
/// reconnect, with a signature tied to this connection's TLS exporter and nonce.
/// This avoids relying on platform-specific client-certificate provisioning.
pub async fn authenticate(
    connection: &Connection,
    send: &mut SendStream,
    recv: &mut RecvStream,
    identity: &Identity,
    pairings: &mut Pairings,
    hello: Option<Vec<u8>>,
    pin_admission: &PinAdmission,
    show_pin: impl FnOnce(&str),
) -> Result<Authenticated> {
    let hello = match hello {
        Some(hello) => hello,
        None => tokio::time::timeout(AUTH_TIMEOUT, read_frame(recv)).await??,
    };
    ensure!(hello.len() >= 50 && hello[0] == HELLO, "invalid QUIC hello");
    let client_id = &hello[1..17];
    let client_key: [u8; 32] = hello[17..49].try_into()?;
    let name = std::str::from_utf8(&hello[49..])?.trim();
    ensure!(!name.is_empty() && name.len() <= 63, "invalid client name");
    let id = hex(client_id);
    let known = pairings
        .clients
        .get(&id)
        .is_some_and(|p| p.public_key == client_key);
    let sid = cpace::random::<32>();
    let fingerprint = identity.fingerprint();
    let context = binding(
        client_id,
        &identity.id,
        &client_key,
        &fingerprint,
        &exporter(connection)?,
    );
    let mut challenge = vec![CHALLENGE, u8::from(known)];
    challenge.extend_from_slice(&identity.id);
    challenge.extend_from_slice(&fingerprint);
    challenge.extend_from_slice(&sid);
    if known {
        write_frame(send, &challenge).await?;
        let proof = tokio::time::timeout(AUTH_TIMEOUT, read_frame(recv)).await??;
        ensure!(
            proof.len() == 65 && proof[0] == PROOF,
            "invalid reconnect proof"
        );
        signature::UnparsedPublicKey::new(&signature::ED25519, client_key)
            .verify(
                &cpace::lv(&[b"screx-quic-reconnect-v1", &sid, &context]),
                &proof[1..],
            )
            .map_err(|_| anyhow::anyhow!("client identity proof failed"))?;
    } else {
        let _pin_slot = pin_admission.acquire()?;
        // Another pairing may have completed after this connection loaded the file.
        *pairings = Pairings::load(pairings.path.parent().context("missing pairing directory")?)?;
        // A challenge gets exactly one PAKE share and confirmation attempt.
        // Failure/expiry ends this connection and consumes this PIN.
        let pin = cpace::pin();
        let exchange = cpace::Exchange::new(pin.as_bytes(), &context, &sid);
        let yb = exchange.share;
        challenge.extend_from_slice(&yb);
        write_frame(send, &challenge).await?;
        show_pin(&pin);
        let proof = tokio::time::timeout(PAIR_TIMEOUT, read_frame(recv)).await??;
        ensure!(
            proof.len() == 65 && proof[0] == PAIR,
            "invalid pairing proof"
        );
        let ya: [u8; 32] = proof[1..33].try_into()?;
        let key = exchange.finish(ya, &sid, &ya, client_id, &yb, &identity.id)?;
        cpace::verify(&key, b"client", &context, &proof[33..])?;
        let mut response = vec![PAIRED];
        response.extend(cpace::confirm(&key, b"server", &context));
        write_frame(send, &response).await?;
        pairings.clients.insert(
            id.clone(),
            PairedClient {
                public_key: client_key,
                name: name.into(),
                paired_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs(),
            },
        );
        pairings.save()?;
    }
    ensure!(
        connection.max_datagram_size().unwrap_or(0) >= CHUNK_PAYLOAD + MEDIA_HEADER_LEN,
        "peer QUIC datagram limit is too small"
    );
    // The caller answers READY or BUSY once it knows whether the session is free.
    Ok(Authenticated {
        device_id: id,
        name: name.into(),
    })
}

struct AuthenticationStream {
    send: SendStream,
    recv: RecvStream,
    hello: Vec<u8>,
    // Keep the unused Apple bootstrap stream alive until the session ends.
    _placeholder: Option<(SendStream, RecvStream)>,
}

async fn authentication_stream(connection: &Connection) -> Result<AuthenticationStream> {
    let (send, mut recv) = connection.accept_bi().await?;
    tokio::select! {
        hello = read_frame(&mut recv) => Ok(AuthenticationStream {
            send, recv, hello: hello?, _placeholder: None,
        }),
        second = connection.accept_bi() => {
            let (second_send, mut second_recv) = second?;
            let hello = read_frame(&mut second_recv).await?;
            Ok(AuthenticationStream {
                send: second_send, recv: second_recv, hello,
                _placeholder: Some((send, recv)),
            })
        }
    }
}

/// Snapshot of QUIC path metrics used by adaptive video rate control.
#[derive(Debug, Clone, Copy)]
pub struct TransportFeedback {
    pub rtt: Duration,
    pub min_rtt: Duration,
    pub cwnd: u64,
    pub lost_packets: u64,
    pub congestion_events: u64,
}

/// Session-bound handle for synchronous capture/audio workers.
/// QUIC owns the socket, TLS keys, congestion control and connection migration.
#[derive(Clone)]
pub struct MediaTransport {
    connection: Option<Connection>,
    #[cfg(test)]
    capture: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
}
impl MediaTransport {
    pub fn new(connection: Connection) -> Self {
        Self {
            connection: Some(connection),
            #[cfg(test)]
            capture: None,
        }
    }
    #[cfg(test)]
    pub fn capture() -> (Self, std::sync::mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(1024);
        (
            Self {
                connection: None,
                capture: Some(tx),
            },
            rx,
        )
    }
    pub fn max_datagram_size(&self) -> usize {
        self.connection
            .as_ref()
            .map_or(1200, |c| c.max_datagram_size().unwrap_or(0))
    }
    pub fn transport_feedback(&self) -> Option<TransportFeedback> {
        let connection = self.connection.as_ref()?;
        let stats = connection.stats();
        Some(TransportFeedback {
            rtt: stats.path.rtt,
            min_rtt: stats.path.min_rtt,
            cwnd: stats.path.cwnd,
            lost_packets: stats.path.lost_packets,
            congestion_events: stats.path.congestion_events,
        })
    }
    pub fn send(&self, body: &[u8]) -> Result<()> {
        ensure!(
            body.len() <= self.max_datagram_size(),
            "media datagram exceeds path limit"
        );
        #[cfg(test)]
        if let Some(capture) = &self.capture {
            capture.send(body.to_vec())?;
            return Ok(());
        }
        self.connection
            .as_ref()
            .context("QUIC connection unavailable")?
            .send_datagram(bytes::Bytes::copy_from_slice(body))?;
        Ok(())
    }
    /// Capture workers run on dedicated OS threads. Backpressure them rather
    /// than evicting earlier shards from the QUIC send buffer. Reserve room for
    /// audio; sound must not wait behind an entire keyframe. Returns how long
    /// the call blocked waiting for send buffer space.
    pub fn send_video(
        &self,
        body: &[u8],
        stop: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<Duration> {
        let check_stop = || {
            ensure!(
                !stop.is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed)),
                "video sender stopped"
            );
            Ok::<_, anyhow::Error>(())
        };
        check_stop()?;
        let mut waited = Duration::ZERO;
        if let Some(connection) = &self.connection {
            let mut wait_start: Option<std::time::Instant> = None;
            while connection.datagram_send_buffer_space() < body.len() + 8192 {
                if wait_start.is_none() {
                    wait_start = Some(std::time::Instant::now());
                }
                check_stop()?;
                if let Some(error) = connection.close_reason() {
                    return Err(error.into());
                }
                std::thread::sleep(Duration::from_micros(200));
            }
            if let Some(start) = wait_start {
                waited = start.elapsed();
            }
        }
        self.send(body)?;
        Ok(waited)
    }
    pub fn close(&self) {
        if let Some(connection) = &self.connection {
            connection.close(0u32.into(), b"session ended");
        }
    }

    pub fn is_closed(&self) -> bool {
        self.connection.as_ref().is_some_and(|c| c.close_reason().is_some())
    }
}

/// The live session's device and connection, so the same device can replace it.
type ActiveSession = Arc<std::sync::Mutex<Option<(String, Connection)>>>;

pub struct PinAdmission {
    slot: std::sync::Mutex<Option<tokio::time::Instant>>,
    pending: tokio::sync::Semaphore,
}

impl PinAdmission {
    fn new() -> Self {
        Self { slot: Default::default(), pending: tokio::sync::Semaphore::new(1) }
    }

    fn acquire(&self) -> Result<tokio::sync::SemaphorePermit<'_>> {
        let permit = self.pending.try_acquire().context("another PIN pairing is pending")?;
        let mut last = self.slot.lock().unwrap();
        ensure!(last.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)), "PIN pairing rate limited");
        *last = Some(tokio::time::Instant::now());
        Ok(permit)
    }
}

pub async fn serve(
    endpoint: Endpoint,
    identity: Identity,
    directory: PathBuf,
    shared: Arc<crate::stream_server::SharedState>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    println!(
        "[quic] listening on {} (QUIC v1, TLS 1.3)",
        endpoint.local_addr()?
    );
    let identity = Arc::new(identity);
    let authenticating = Arc::new(tokio::sync::Semaphore::new(16));
    let pairing = Arc::new(PinAdmission::new());
    let mut sources = HashMap::<_, std::sync::Weak<tokio::sync::Semaphore>>::new();
    let active = ActiveSession::default();
    while !stop.load(Ordering::Relaxed) {
        let Some(incoming) = endpoint.accept().await else {
            break;
        };
        if !incoming.remote_address_validated() && incoming.may_retry() {
            let _ = incoming.retry();
            continue;
        }
        let Ok(permit) = Arc::clone(&authenticating).try_acquire_owned() else {
            incoming.refuse();
            continue;
        };
        sources.retain(|_, source| source.strong_count() != 0);
        let ip = incoming.remote_address().ip();
        let source = sources.get(&ip).and_then(std::sync::Weak::upgrade).unwrap_or_else(|| {
            let source = Arc::new(tokio::sync::Semaphore::new(2));
            sources.insert(ip, Arc::downgrade(&source));
            source
        });
        let Ok(source_permit) = source.try_acquire_owned() else {
            incoming.refuse();
            continue;
        };
        // Each connection runs on its own task: a running session must not stop another
        // device from hearing "busy", or the same device from reconnecting.
        tokio::spawn(connection(
            incoming,
            (permit, source_permit),
            Arc::clone(&pairing),
            Arc::clone(&identity),
            directory.clone(),
            Arc::clone(&shared),
            Arc::clone(&active),
        ));
    }
    endpoint.close(0u32.into(), b"shutdown");
    Ok(())
}

async fn connection(
    incoming: quinn::Incoming,
    permits: (tokio::sync::OwnedSemaphorePermit, tokio::sync::OwnedSemaphorePermit),
    pairing: Arc<PinAdmission>,
    identity: Arc<Identity>,
    directory: PathBuf,
    shared: Arc<crate::stream_server::SharedState>,
    active: ActiveSession,
) {
    use std::sync::atomic::Ordering;
    let Ok(Ok(connection)) = tokio::time::timeout(AUTH_TIMEOUT, incoming).await else {
        return;
    };
    let authenticated = async {
        let AuthenticationStream { send, mut recv, hello, _placeholder } =
            tokio::time::timeout(AUTH_TIMEOUT, authentication_stream(&connection)).await??;
        let mut send = send;
        let mut pairings = Pairings::load(&directory)?;
        let auth = authenticate(
            &connection, &mut send, &mut recv, &identity, &mut pairings, Some(hello), &pairing,
            |pin| println!(
                "\n[pairing] Enter this eight-digit PIN on the iPad: {}-{}\n[pairing] One attempt; expires in 120 seconds.\n",
                &pin[..4], &pin[4..]
            ),
        )
        .await?;
        Ok::<_, anyhow::Error>((auth, send, recv, _placeholder))
    }
    .await;
    drop(permits);
    let (auth, mut send, recv, _placeholder) = match authenticated {
        Ok(value) => value,
        Err(error) => {
            eprintln!("[quic] authentication failed for {}: {error:#}", connection.remote_address());
            connection.close(1u32.into(), b"authentication failed");
            return;
        }
    };
    let label = format!("{} ({})", auth.name, &auth.device_id[..5]);

    let session = match claim_session(&shared, &active, &auth.device_id, &connection).await {
        SessionClaim::Reserved(session) => session,
        SessionClaim::Busy => {
            println!("[quic] {label} rejected: another device is connected");
            if write_frame(&mut send, &[BUSY]).await.is_ok() {
                let _ = tokio::time::timeout(Duration::from_secs(2), connection.closed()).await;
            }
            connection.close(1u32.into(), b"busy");
            return;
        }
        SessionClaim::CleanupTimedOut => {
            println!("[quic] {label}: session cleanup timed out");
            connection.close(1u32.into(), b"session cleanup timed out");
            return;
        }
    };
    println!("[quic] session started with {label}");

    let result = async {
        write_frame(&mut send, &[READY]).await?;
        *shared.quic_media.lock().unwrap() = Some(MediaTransport::new(connection.clone()));
        shared.has_active_client.store(true, Ordering::Release);
        shared.audio_notify.notify_all();
        application(&connection, &shared, session, send, recv).await
    }
    .await;
    match result {
        Ok(()) => println!("[quic] session with {label} ended"),
        Err(error) => println!("[quic] session with {label} ended: {error:#}"),
    }
    connection.close(0u32.into(), b"session ended");
    // Cleanup removes devices and joins worker threads; keep it off the async workers.
    let cleanup = Arc::clone(&shared);
    let _ = tokio::task::spawn_blocking(move || {
        crate::stream_server::drop_network_client(&cleanup, session)
    })
    .await;
    let mut active = active.lock().unwrap();
    if active.as_ref().is_some_and(|(_, c)| c.stable_id() == connection.stable_id()) {
        *active = None;
    }
}

enum SessionClaim {
    Reserved(u64),
    Busy,
    CleanupTimedOut,
}

/// Reserve and publish the owner together. Only the owner can request takeover.
async fn claim_session(
    shared: &crate::stream_server::SharedState,
    active: &ActiveSession,
    device_id: &str,
    connection: &Connection,
) -> SessionClaim {
    let reserve = |owner: &mut Option<(String, Connection)>| {
        shared.reserve_network_session().map(|session| {
            *owner = Some((device_id.to_owned(), connection.clone()));
            SessionClaim::Reserved(session)
        })
    };
    let previous = {
        let mut owner = active.lock().unwrap();
        if let Some(claim) = reserve(&mut owner) {
            return claim;
        }
        match owner.as_ref() {
            Some((id, previous)) if id == device_id => previous.clone(),
            _ => return SessionClaim::Busy,
        }
    };
    previous.close(0u32.into(), b"replaced by a new connection");
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if connection.close_reason().is_some() {
            break;
        }
        if let Some(claim) = reserve(&mut active.lock().unwrap()) {
            return claim;
        }
    }
    SessionClaim::CleanupTimedOut
}

async fn application(
    connection: &Connection,
    shared: &Arc<crate::stream_server::SharedState>,
    session: u64,
    mut send: SendStream,
    mut recv: RecvStream,
) -> Result<()> {
    use crate::protocol;
    // Clear only this session's negotiation mailbox.
    *shared.pending_settings.0.lock().unwrap() = None;
    write_frame(&mut send, &crate::stream_server::build_caps_message(shared)).await?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let reader = tokio::spawn(async move {
        loop {
            let frame = read_frame(&mut recv).await;
            let failed = frame.is_err();
            if tx.send(frame).await.is_err() || failed {
                break;
            }
        }
    });
    let input = tokio::spawn({
        let connection = connection.clone();
        let shared = Arc::clone(shared);
        async move {
            let result = async {
                let (_send, mut recv) = connection.accept_bi().await?;
                loop {
                    let message = read_frame(&mut recv).await?;
                    ensure!(
                        matches!(message[0], 0x10..=0x15),
                        "non-input message on input stream"
                    );
                    // Input handlers only write to non-blocking uinput fds or enqueue onto
                    // KeyboardWorker; running inline avoids per-event spawn_blocking jitter.
                    crate::stream_server::handle_control_message_data(&shared, session, &message);
                }
                #[allow(unreachable_code)]
                Ok::<(), anyhow::Error>(())
            }
            .await;
            if result.is_err() {
                connection.close(2u32.into(), b"input stream ended");
            }
        }
    });
    let datagrams = tokio::spawn({
        let connection = connection.clone();
        let shared = Arc::clone(shared);
        async move {
            let mut camera = crate::camera::CamReassembler::new();
            while let Ok(packet) = connection.read_datagram().await {
                if !shared.is_current_network_session(session) {
                    break;
                }
                let Some((&kind, body)) = packet.split_first() else {
                    continue;
                };
                match kind {
                    protocol::datagram::MIC if body.len() > 4 => {
                        let seq = u32::from_be_bytes(body[..4].try_into().unwrap());
                        if let Ok(mut writer) = shared.mic_writer.try_lock() {
                            if let Some(writer) = writer.as_mut() {
                                let _ = writer.write_opus_packet(seq, &body[4..]);
                            }
                        }
                    }
                    protocol::datagram::CAMERA => {
                        if let Some(jpeg) = camera.feed(body) {
                            if let Some(writer) = shared.cam_writer.lock().unwrap().as_mut() {
                                writer.write_frame(&jpeg);
                            }
                        }
                    }
                    protocol::datagram::AUDIO_REPORT => {
                        if let Some(report) = crate::audio::AudioReport::parse(body) {
                            let mut reports = shared.audio_reports.lock().unwrap();
                            if reports.len() >= 16 {
                                reports.pop_front();
                            }
                            reports.push_back(report);
                        }
                    }
                    protocol::datagram::PLI if body.len() == 5 => {
                        if let Some(display) = crate::capture::DisplayId::from_wire(body[0]) {
                            let generation = u32::from_be_bytes(body[1..5].try_into().unwrap());
                            shared.display_stream(display).request_keyframe(generation);
                        }
                    }
                    _ => {}
                }
            }
        }
    });
    let mut ticker = tokio::time::interval(Duration::from_millis(20));
    let mut last_stats = tokio::time::Instant::now();
    let result = async {
        loop {
            tokio::select! {
                frame = rx.recv() => {
                    let message = frame.context("control stream ended")??;
                    match message[0] {
                        protocol::client::DISCONNECT => break,
                        protocol::client::PING => write_frame(&mut send, &[protocol::daemon::PONG]).await?,
                        0x10..=0x15 => anyhow::bail!("input must use the input stream"),
                        _ => {
                            let shared = Arc::clone(shared);
                            tokio::task::spawn_blocking(move || {
                                crate::stream_server::handle_control_message_data(&shared, session, &message);
                            }).await?;
                        }
                    }
                }
                _ = ticker.tick() => {
                    ensure!(shared.is_current_network_session(session), "session retired");
                    if last_stats.elapsed() >= Duration::from_secs(5) {
                        let stats = connection.stats();
                        crate::log_detail!(
                            "[quic] peer={} rtt={:.1} ms min_rtt={:.1} ms cwnd={} bytes mtu={} datagram_limit={} sent={} lost={} congestion_events={}",
                            connection.remote_address(),
                            stats.path.rtt.as_secs_f64() * 1000.0,
                            stats.path.min_rtt.as_secs_f64() * 1000.0,
                            stats.path.cwnd, stats.path.current_mtu,
                            connection.max_datagram_size().unwrap_or(0),
                            stats.path.sent_packets, stats.path.lost_packets, stats.path.congestion_events,
                        );
                        last_stats = tokio::time::Instant::now();
                    }
                    let statuses = crate::stream_server::take_session_messages(
                        &mut shared.display_status.lock().unwrap(), session);
                    for status in statuses { write_frame(&mut send, &status).await?; }
                }
                error = connection.closed() => return Err(error.into()),
            }
        }
        Ok(())
    }.await;
    connection.close(0u32.into(), b"session ended");
    reader.abort();
    datagrams.abort();
    // Wait for in-flight input before the disconnect callback releases device state.
    let _ = input.await;
    let _ = datagrams.await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::KeyPair;

    fn directory() -> PathBuf {
        let path = PathBuf::from("/tmp/opencode")
            .join(format!("screx-quic-test-{}", hex(&cpace::random::<8>())));
        fs::create_dir_all(&path).unwrap();
        path
    }
    fn client(identity: &Identity) -> Endpoint {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(identity.certificate.clone()))
            .unwrap();
        let mut tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
        let config = quinn::ClientConfig::new(Arc::new(crypto));
        let mut endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(config);
        endpoint
    }

    /// Manual interoperability companion for Apple's Network.framework client.
    /// No real display, input device, audio card or production pairing files.
    #[tokio::test]
    #[ignore = "requires an Apple client on the LAN"]
    async fn apple_interop_server() {
        let directory = directory();
        let identity = Identity::load(&directory).unwrap();
        let server = identity.endpoint("0.0.0.0:19001".parse().unwrap()).unwrap();
        println!("INTEROP_LISTEN");
        let exchange = async {
            let connection = server.accept().await.unwrap().await.unwrap();
            let AuthenticationStream { mut send, mut recv, hello, _placeholder } =
                authentication_stream(&connection).await.unwrap();
            let mut pairings = Pairings::load(&directory).unwrap();
            authenticate(&connection, &mut send, &mut recv, &identity, &mut pairings, Some(hello), &PinAdmission::new(),
                |pin| println!("INTEROP_PIN {pin}")).await.unwrap();
            write_frame(&mut send, &[READY]).await.unwrap();
            assert_eq!(read_frame(&mut recv).await.unwrap(), b"ping");
            write_frame(&mut send, b"pong").await.unwrap();
            let (mut input_send, mut input_recv) = connection.accept_bi().await.unwrap();
            assert_eq!(read_frame(&mut input_recv).await.unwrap(), b"input");
            write_frame(&mut input_send, b"input-ok").await.unwrap();
            let packet = connection.read_datagram().await.unwrap();
            assert_eq!(packet.len(), 1022);
            connection.send_datagram(packet).unwrap();
            assert_eq!(read_frame(&mut recv).await.unwrap(), b"done");
            println!("INTEROP_DONE");
        };
        tokio::time::timeout(Duration::from_secs(90), exchange).await.unwrap();
    }
    async fn login(
        connection: &Connection,
        signing: &signature::Ed25519KeyPair,
        pin: Option<String>,
    ) -> Result<(SendStream, RecvStream)> {
        let (mut send, mut recv) = connection.open_bi().await?;
        let client_id = [42; 16];
        let mut hello = vec![HELLO];
        hello.extend(client_id);
        hello.extend(signing.public_key().as_ref());
        hello.extend(b"Test iPad");
        write_frame(&mut send, &hello).await?;
        let challenge = read_frame(&mut recv).await?;
        ensure!(challenge[0] == CHALLENGE, "challenge missing");
        let server_id = &challenge[2..18];
        let sid = &challenge[50..82];
        let context = binding(
            &client_id,
            server_id,
            signing.public_key().as_ref(),
            &challenge[18..50],
            &exporter(connection)?,
        );
        if challenge[1] == 1 {
            let mut proof = vec![PROOF];
            proof.extend(
                signing
                    .sign(&cpace::lv(&[b"screx-quic-reconnect-v1", sid, &context]))
                    .as_ref(),
            );
            write_frame(&mut send, &proof).await?;
        } else {
            let exchange =
                cpace::Exchange::new(pin.context("PIN required")?.as_bytes(), &context, sid);
            let ya = exchange.share;
            let yb = challenge[82..114].try_into()?;
            let key = exchange.finish(yb, sid, &ya, &client_id, &yb, server_id)?;
            let mut proof = vec![PAIR];
            proof.extend(ya);
            proof.extend(cpace::confirm(&key, b"client", &context));
            write_frame(&mut send, &proof).await?;
            let result = read_frame(&mut recv).await?;
            ensure!(result[0] == PAIRED, "pair confirmation missing");
            cpace::verify(&key, b"server", &context, &result[1..])?;
        }
        ensure!(read_frame(&mut recv).await? == [READY], "not ready");
        Ok((send, recv))
    }

    /// Reconnects as an already paired device and returns the frame that follows
    /// authentication (READY or BUSY), keeping the control stream open.
    async fn reconnect(
        endpoint: &Endpoint,
        address: std::net::SocketAddr,
        id: [u8; 16],
        signing: &signature::Ed25519KeyPair,
    ) -> (Connection, Vec<u8>, SendStream, RecvStream) {
        let connection = endpoint.connect(address, "screx.local").unwrap().await.unwrap();
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        let mut hello = vec![HELLO];
        hello.extend(id);
        hello.extend(signing.public_key().as_ref());
        hello.extend(b"Test iPad");
        write_frame(&mut send, &hello).await.unwrap();
        let challenge = read_frame(&mut recv).await.unwrap();
        assert_eq!(challenge[..2], [CHALLENGE, 1], "device should be known");
        let context = binding(
            &id,
            &challenge[2..18],
            signing.public_key().as_ref(),
            &challenge[18..50],
            &exporter(&connection).unwrap(),
        );
        let mut proof = vec![PROOF];
        proof.extend(
            signing
                .sign(&cpace::lv(&[b"screx-quic-reconnect-v1", &challenge[50..82], &context]))
                .as_ref(),
        );
        write_frame(&mut send, &proof).await.unwrap();
        let reply = read_frame(&mut recv).await.unwrap();
        (connection, reply, send, recv)
    }

    #[tokio::test]
    async fn another_device_is_busy_and_the_same_device_replaces_its_session() {
        let directory = directory();
        let identity = Identity::load(&directory).unwrap();
        let a = signature::Ed25519KeyPair::from_seed_unchecked(&[1; 32]).unwrap();
        let b = signature::Ed25519KeyPair::from_seed_unchecked(&[2; 32]).unwrap();
        let mut pairings = Pairings::load(&directory).unwrap();
        for (id, key) in [([1u8; 16], &a), ([2u8; 16], &b)] {
            pairings.clients.insert(
                hex(&id),
                PairedClient {
                    public_key: key.public_key().as_ref().try_into().unwrap(),
                    name: "Test iPad".into(),
                    paired_at: 0,
                },
            );
        }
        pairings.save().unwrap();
        let server = identity.endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = server.local_addr().unwrap();
        let endpoint = client(&identity);
        let shared = crate::test_support::shared_state();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = tokio::spawn(serve(server, identity, directory, Arc::clone(&shared), stop));
        // Leave a legitimate PIN prompt open while testing known-device admission.
        let pairing = endpoint.connect(address, "screx.local").unwrap().await.unwrap();
        let (mut pending_send, mut pending_recv) = pairing.open_bi().await.unwrap();
        let mut hello = vec![HELLO];
        hello.extend([3; 16]);
        hello.extend(a.public_key().as_ref());
        hello.extend(b"New iPad");
        write_frame(&mut pending_send, &hello).await.unwrap();
        let challenge = read_frame(&mut pending_recv).await.unwrap();
        assert_eq!(challenge[..2], [CHALLENGE, 0]);

        let (first, reply, _s1, _r1) = reconnect(&endpoint, address, [1; 16], &a).await;
        assert_eq!(reply, [READY]);
        let (_other, reply, _s2, _r2) = reconnect(&endpoint, address, [2; 16], &b).await;
        assert_eq!(reply, [BUSY], "another device must hear busy, not time out");
        let (_second, reply, _s3, _r3) = reconnect(&endpoint, address, [1; 16], &a).await;
        assert_eq!(reply, [READY], "the same device replaces its own session");
        tokio::time::timeout(Duration::from_secs(5), first.closed()).await.unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn pairs_then_reconnects_and_carries_streams_and_datagrams() {
        let directory = directory();
        let identity = Identity::load(&directory).unwrap();
        let saved = Identity::load(&directory).unwrap();
        assert_eq!(identity.certificate, saved.certificate);
        let server = identity.endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        let client = client(&identity);
        let addr = server.local_addr().unwrap();
        let key = signature::Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap();
        let (pin_tx, mut pin_rx) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(async move {
            let mut pairings = Pairings::load(&directory).unwrap();
            for _ in 0..2 {
                let connection = server.accept().await.unwrap().await.unwrap();
                let AuthenticationStream { mut send, mut recv, hello, _placeholder } =
                    authentication_stream(&connection).await.unwrap();
                let tx = pin_tx.clone();
                let auth = authenticate(
                    &connection,
                    &mut send,
                    &mut recv,
                    &identity,
                    &mut pairings,
                    Some(hello),
                    &PinAdmission::new(),
                    move |pin| {
                        tx.try_send(pin.to_owned()).unwrap();
                    },
                )
                .await
                .unwrap();
                assert_eq!(auth.name, "Test iPad");
                assert_eq!(auth.device_id, hex(&[42; 16]));
                write_frame(&mut send, &[READY]).await.unwrap();
                let (mut input_send, mut input_recv) = connection.accept_bi().await.unwrap();
                let input = read_frame(&mut input_recv).await.unwrap();
                write_frame(&mut input_send, &input).await.unwrap();
                let media = connection.read_datagram().await.unwrap();
                connection.send_datagram(media).unwrap();
                // Wait for client to receive, not just queue, both replies.
                assert_eq!(read_frame(&mut recv).await.unwrap(), b"done");
                write_frame(&mut send, b"done").await.unwrap();
                // Wait for this connection to close before accepting the next.
                connection.closed().await;
            }
            assert_eq!(Pairings::load(&directory).unwrap().clients.len(), 1);
            server.close(0u32.into(), b"test complete");
        });
        for first in [true, false] {
            let connection = client.connect(addr, "screx.local").unwrap().await.unwrap();
            // Model Network.framework reserving an unused initial stream. A
            // partial header keeps it idle while the real control flow opens.
            let _bootstrap = if first {
                let (mut send, recv) = connection.open_bi().await.unwrap();
                send.write_all(&[0]).await.unwrap();
                Some((send, recv))
            } else { None };
            let pin = if first {
                // Server prints a PIN only after HELLO: start login and let this task feed
                // it via an explicit handshake below.
                Some(())
            } else {
                None
            };
            // A login future needs the generated PIN, so drive HELLO separately on first
            // connect by pre-arranging the server's displayed code through a task.
            if pin.is_some() {
                let (mut send, mut recv) = connection.open_bi().await.unwrap();
                let mut hello = vec![HELLO];
                hello.extend([42; 16]);
                hello.extend(key.public_key().as_ref());
                hello.extend(b"Test iPad");
                write_frame(&mut send, &hello).await.unwrap();
                let challenge = read_frame(&mut recv).await.unwrap();
                let code = pin_rx.recv().await.unwrap();
                let sid = &challenge[50..82];
                let server_id = &challenge[2..18];
                let context = binding(
                    &[42; 16],
                    server_id,
                    key.public_key().as_ref(),
                    &challenge[18..50],
                    &exporter(&connection).unwrap(),
                );
                let exchange = cpace::Exchange::new(code.as_bytes(), &context, sid);
                let ya = exchange.share;
                let yb = challenge[82..114].try_into().unwrap();
                let secret = exchange
                    .finish(yb, sid, &ya, &[42; 16], &yb, server_id)
                    .unwrap();
                let mut proof = vec![PAIR];
                proof.extend(ya);
                proof.extend(cpace::confirm(&secret, b"client", &context));
                write_frame(&mut send, &proof).await.unwrap();
                let result = read_frame(&mut recv).await.unwrap();
                cpace::verify(&secret, b"server", &context, &result[1..]).unwrap();
                assert_eq!(read_frame(&mut recv).await.unwrap(), [READY]);
                exchange_traffic(&connection).await;
                write_frame(&mut send, b"done").await.unwrap();
                assert_eq!(read_frame(&mut recv).await.unwrap(), b"done");
                connection.close(0u32.into(), b"done");
            } else {
                let (mut send, mut recv) = login(&connection, &key, None).await.unwrap();
                exchange_traffic(&connection).await;
                write_frame(&mut send, b"done").await.unwrap();
                assert_eq!(read_frame(&mut recv).await.unwrap(), b"done");
                connection.close(0u32.into(), b"done");
            }
        }
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
    }

    async fn exchange_traffic(connection: &Connection) {
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        write_frame(&mut send, b"input").await.unwrap();
        assert_eq!(read_frame(&mut recv).await.unwrap(), b"input");
        let media = vec![0x5a; CHUNK_PAYLOAD + MEDIA_HEADER_LEN];
        connection.send_datagram(media.clone().into()).unwrap();
        assert_eq!(connection.read_datagram().await.unwrap().as_ref(), &media);
    }

    #[tokio::test]
    async fn incorrect_pin_does_not_create_a_pairing() {
        let directory = directory();
        let identity = Identity::load(&directory).unwrap();
        let server = identity.endpoint("127.0.0.1:0".parse().unwrap()).unwrap();
        let endpoint = client(&identity);
        let address = server.local_addr().unwrap();
        let (pin_tx, pin_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let connection = server.accept().await.unwrap().await.unwrap();
            let (mut send, mut recv) = connection.accept_bi().await.unwrap();
            let mut pairings = Pairings::load(&directory).unwrap();
            let result = authenticate(
                &connection,
                &mut send,
                &mut recv,
                &identity,
                &mut pairings,
                None,
                &PinAdmission::new(),
                |pin| {
                    let _ = pin_tx.send(pin.to_owned());
                },
            )
            .await;
            assert!(result.is_err());
            assert!(pairings.clients.is_empty());
            assert!(Pairings::load(&directory).unwrap().clients.is_empty());
            connection.close(1u32.into(), b"authentication failed");
            server.close(0u32.into(), b"done");
        });
        let connection = endpoint
            .connect(address, "screx.local")
            .unwrap()
            .await
            .unwrap();
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        let signing = signature::Ed25519KeyPair::from_seed_unchecked(&[9; 32]).unwrap();
        let mut hello = vec![HELLO];
        hello.extend([43; 16]);
        hello.extend(signing.public_key().as_ref());
        hello.extend(b"Wrong PIN client");
        write_frame(&mut send, &hello).await.unwrap();
        let challenge = read_frame(&mut recv).await.unwrap();
        let correct = pin_rx.await.unwrap();
        let wrong = if correct == "00000000" {
            "00000001"
        } else {
            "00000000"
        };
        let sid = &challenge[50..82];
        let server_id = &challenge[2..18];
        let context = binding(
            &[43; 16],
            server_id,
            signing.public_key().as_ref(),
            &challenge[18..50],
            &exporter(&connection).unwrap(),
        );
        let exchange = cpace::Exchange::new(wrong.as_bytes(), &context, sid);
        let ya = exchange.share;
        let yb = challenge[82..114].try_into().unwrap();
        let key = exchange
            .finish(yb, sid, &ya, &[43; 16], &yb, server_id)
            .unwrap();
        let mut proof = vec![PAIR];
        proof.extend(ya);
        proof.extend(cpace::confirm(&key, b"client", &context));
        write_frame(&mut send, &proof).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn wildcard_ipv6_listener_accepts_ipv4() {
        let directory = directory();
        let identity = Identity::load(&directory).unwrap();
        let server = identity.endpoint("[::]:0".parse().unwrap()).unwrap();
        let endpoint = client(&identity);
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], server.local_addr().unwrap().port()));
        let connecting = endpoint.connect(address, "screx.local").unwrap();
        let accept = async { server.accept().await.unwrap().await.unwrap() };
        let (client_connection, server_connection) = tokio::join!(connecting, accept);
        client_connection.unwrap().close(0u32.into(), b"test complete");
        server_connection.close(0u32.into(), b"test complete");
    }
}
