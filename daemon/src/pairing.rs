use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use ring::agreement::{self, EphemeralPrivateKey, UnparsedPublicKey, X25519};
use ring::rand::SystemRandom;
use serde::{Deserialize, Serialize};

use crate::crypto;
use crate::platform::shim::{config_dir, hostname};

// Wire protocol magic bytes
const MAGIC_PAIR: &[u8] = b"SCREX_PAIR"; // 10 bytes
const MAGIC_HELLO: &[u8] = b"SCREX_HELLO"; // 11 bytes
const MAGIC_PIN: &[u8] = b"SCREX_PIN\0"; // 10 bytes (padded for alignment)
const MAGIC_ANSWER: &[u8] = b"SCREX_ANSWER"; // 12 bytes
const MAGIC_OK: &[u8] = b"SCREX_OK\0\0"; // 10 bytes
const MAGIC_REJECT: &[u8] = b"SCREX_REJECT"; // 12 bytes

const DEVICE_ID_LEN: usize = 16;
const PUBKEY_LEN: usize = 32;
const NONCE_LEN: usize = 32;
const HMAC_LEN: usize = 32;
const PIN_DIGITS: usize = 6;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PairedDevice {
    pairing_key: String, // hex-encoded 32-byte key
    name: String,
    paired_at: String,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: u64,
    pub session_key: [u8; 32],
    pub client_addr: std::net::SocketAddr,
    /// Local IP the client dialed for the TCP handshake; used to pin the
    /// source address of daemon→client UDP on multi-homed hosts.
    pub local_ip: Option<std::net::IpAddr>,
}

pub struct PairingState {
    paired_devices: HashMap<String, PairedDevice>, // device_id (hex) -> PairedDevice
    config_path: PathBuf,
    daemon_device_id: [u8; DEVICE_ID_LEN],
    pending_pin: Mutex<Option<PendingPairing>>,
}

#[allow(dead_code)]
struct PendingPairing {
    pin: String,
    device_id: [u8; DEVICE_ID_LEN],
    ecdh_secret: Vec<u8>,
}

impl PairingState {
    pub fn load() -> Self {
        let config_path = config_dir().join("paired_devices.json");
        let paired_devices = if config_path.exists() {
            match fs::read_to_string(&config_path) {
                Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
                Err(_) => HashMap::new(),
            }
        } else {
            HashMap::new()
        };

        let count = paired_devices.len();
        if count > 0 {
            println!("[pairing] loaded {count} paired device(s)");
        }

        let daemon_id_path = config_dir().join("daemon_id");
        let daemon_device_id = load_or_create_daemon_id(&daemon_id_path);
        println!(
            "[pairing] daemon device id: {}",
            hex_encode(&daemon_device_id)
        );

        Self {
            paired_devices,
            config_path,
            daemon_device_id,
            pending_pin: Mutex::new(None),
        }
    }

    pub fn daemon_device_id(&self) -> [u8; DEVICE_ID_LEN] {
        self.daemon_device_id
    }

    fn save(&self) {
        if let Some(parent) = self.config_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(&self.paired_devices) {
            Ok(json) => {
                if let Err(e) = fs::write(&self.config_path, json) {
                    eprintln!("[pairing] failed to save paired devices: {e}");
                }
            }
            Err(e) => eprintln!("[pairing] serialize error: {e}"),
        }
    }

    fn is_paired(&self, device_id: &str) -> bool {
        self.paired_devices.contains_key(device_id)
    }

    fn get_pairing_key(&self, device_id: &str) -> Option<[u8; 32]> {
        self.paired_devices.get(device_id).and_then(|dev| {
            let bytes = hex_decode(&dev.pairing_key)?;
            if bytes.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(&bytes);
                Some(key)
            } else {
                None
            }
        })
    }

    fn store_device(
        &mut self,
        device_id: &[u8; DEVICE_ID_LEN],
        pairing_key: &[u8; 32],
        name: &str,
    ) {
        let id_hex = hex_encode(device_id);
        self.paired_devices.insert(
            id_hex,
            PairedDevice {
                pairing_key: hex_encode(pairing_key),
                name: name.to_string(),
                paired_at: chrono_now(),
            },
        );
        self.save();
    }

    pub fn remove_device(&mut self, device_id: &str) -> bool {
        if self.paired_devices.remove(device_id).is_some() {
            self.save();
            true
        } else {
            false
        }
    }

    pub fn remove_all(&mut self) -> usize {
        let count = self.paired_devices.len();
        self.paired_devices.clear();
        self.save();
        count
    }

    pub fn list_devices(&self) -> Vec<(String, String, String)> {
        self.paired_devices
            .iter()
            .map(|(id, dev)| (id.clone(), dev.name.clone(), dev.paired_at.clone()))
            .collect()
    }
}

fn load_or_create_daemon_id(path: &PathBuf) -> [u8; DEVICE_ID_LEN] {
    if let Ok(hex) = fs::read_to_string(path) {
        let hex = hex.trim();
        if hex.len() == DEVICE_ID_LEN * 2 {
            if let Some(bytes) = hex_decode(hex) {
                if bytes.len() == DEVICE_ID_LEN {
                    let mut id = [0u8; DEVICE_ID_LEN];
                    id.copy_from_slice(&bytes);
                    return id;
                }
            }
        }
    }

    let rng = SystemRandom::new();
    let mut id = [0u8; DEVICE_ID_LEN];
    rand_bytes_into(&rng, &mut id);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, hex_encode(&id));
    id
}

fn rand_bytes_into(rng: &SystemRandom, buf: &mut [u8]) {
    ring::rand::SecureRandom::fill(rng, buf).expect("rng fill");
}

const MAGIC_BUSY: &[u8] = b"SCREX_BUSY\0\0"; // 12 bytes
const CONTROL_MAX_FRAME: usize = 65536;
const CONTROL_READ_TIMEOUT: Duration = Duration::from_millis(200);
const CONTROL_DISCONNECT_MAGIC: &[u8] = b"DISCONNECT";
const CONTROL_HOSTNAME_MAGIC: &[u8] = b"HOST";

/// Run the TCP pairing/handshake server. Blocks until `stop` is set.
/// On successful handshake, pushes a `SessionInfo` into `session_tx`.
/// Rejects connections when a session is already active (single-client mode).
pub fn run_pairing_server(
    port: u16,
    pairing: Arc<Mutex<PairingState>>,
    session_tx: Arc<Mutex<Option<SessionInfo>>>,
    shared: Arc<crate::stream_server::SharedState>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .with_context(|| format!("failed to bind TCP pairing port {port}"))?;
    listener
        .set_nonblocking(true)
        .context("set TCP listener nonblocking")?;

    println!("[pairing] TCP handshake server listening on port {port}");

    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut stream, addr)) => {
                // On Windows, an accepted socket may inherit the listener's non-blocking
                // flag. Handshake and control loops expect a blocking socket.
                stream.set_nonblocking(false).ok();

                // Reject if a session is already active (single-client mode)
                if shared.has_active_client.load(Ordering::Relaxed) {
                    println!("[pairing] rejecting {addr} — session already active");
                    let _ = stream.write_all(MAGIC_BUSY);
                    let _ = stream.flush();
                    continue;
                }

                let reserved_session_id = match shared.reserve_network_session() {
                    Some(id) => id,
                    None => {
                        println!("[pairing] rejecting {addr} — network session already pending");
                        let _ = stream.write_all(MAGIC_BUSY);
                        let _ = stream.flush();
                        continue;
                    }
                };

                println!("[pairing] incoming handshake from {addr}");
                stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
                stream.set_write_timeout(Some(Duration::from_secs(10))).ok();

                match handle_handshake(&mut stream, addr, &pairing) {
                    Ok(mut session) => {
                        session.session_id = reserved_session_id;
                        session.local_ip = stream.local_addr().ok().map(|a| a.ip());
                        println!("[pairing] handshake with {addr} completed");
                        *session_tx.lock().unwrap() = Some(session.clone());
                        let shared_control = Arc::clone(&shared);
                        let stop_control = Arc::clone(&stop);
                        let session_id = session.session_id;
                        let session_key = session.session_key;
                        if let Err(e) = std::thread::Builder::new()
                            .name(format!("control-{session_id}"))
                            .spawn(move || {
                                if let Err(e) = run_control_loop(
                                    stream,
                                    &shared_control,
                                    &stop_control,
                                    session_key,
                                    session_id,
                                ) {
                                    eprintln!("[control] network control loop ended: {e:#}");
                                }
                            })
                        {
                            eprintln!("[control] failed to spawn control loop: {e}");
                            crate::stream_server::drop_network_client(&shared, reserved_session_id);
                        }
                    }
                    Err(e) => {
                        crate::stream_server::drop_network_client(&shared, reserved_session_id);
                        eprintln!("[pairing] handshake with {addr} failed: {e:#}");
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("[pairing] accept error: {e}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }

    println!("[pairing] handshake server stopped");
    Ok(())
}

fn handle_handshake(
    stream: &mut TcpStream,
    addr: std::net::SocketAddr,
    pairing: &Arc<Mutex<PairingState>>,
) -> Result<SessionInfo> {
    // The handshake may require more than one round trip. If a client sends
    // HELLO with a stale/missing key, we send REJECT and keep the connection
    // open so the client can immediately retry with PAIR on the same socket.
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        if attempts > 3 {
            anyhow::bail!("too many handshake attempts from {addr}");
        }

        let mut header = [0u8; 12]; // max magic length
        stream.read_exact(&mut header)?;

        if header[..MAGIC_PAIR.len()] == *MAGIC_PAIR {
            crate::vlog!("[pairing] handshake type=PAIR from {addr}");
            return handle_pair_request(stream, addr, &header, pairing);
        } else if header[..MAGIC_HELLO.len()] == *MAGIC_HELLO {
            crate::vlog!("[pairing] handshake type=HELLO from {addr}");
            match handle_hello_request(stream, addr, &header, pairing)? {
                Some(session) => return Ok(session),
                None => {
                    // REJECT sent; loop back to read the client's PAIR retry.
                    continue;
                }
            }
        } else {
            anyhow::bail!("unknown handshake magic");
        }
    }
}

fn handle_pair_request(
    stream: &mut TcpStream,
    addr: std::net::SocketAddr,
    header_buf: &[u8; 12],
    pairing: &Arc<Mutex<PairingState>>,
) -> Result<SessionInfo> {
    // Already read 12 bytes; SCREX_PAIR is 10, so 2 bytes of device_id are in header_buf[10..12]
    // Read remaining: device_id(14 more) + client_pubkey(32) = 46 bytes
    let mut rest = [0u8; 46];
    stream.read_exact(&mut rest)?;

    let mut device_id = [0u8; DEVICE_ID_LEN];
    device_id[..2].copy_from_slice(&header_buf[10..12]);
    device_id[2..].copy_from_slice(&rest[..14]);

    let mut client_pubkey = [0u8; PUBKEY_LEN];
    client_pubkey.copy_from_slice(&rest[14..46]);

    let device_id_hex = hex_encode(&device_id);
    println!("[pairing] pair request from device {device_id_hex}");

    // A PAIR request always means the client wants to run the full PIN pairing
    // flow. This happens on first pairing, after the client lost its key, or
    // when connecting from a new IP address. Upgrading to a session from a
    // PAIR request is unsafe because the client may not have the pairing key.
    {
        let ps = pairing.lock().unwrap();
        if ps.is_paired(&device_id_hex) {
            crate::vlog!("[pairing] device {device_id_hex} already paired; re-pairing");
        }
    }

    // Generate server ECDH keypair
    let rng = SystemRandom::new();
    let server_private = EphemeralPrivateKey::generate(&X25519, &rng)
        .map_err(|e| anyhow::anyhow!("X25519 keygen: {e}"))?;
    let server_public = server_private
        .compute_public_key()
        .map_err(|e| anyhow::anyhow!("X25519 pubkey: {e}"))?;

    // Compute ECDH shared secret
    let client_pub = UnparsedPublicKey::new(&X25519, &client_pubkey);
    let ecdh_secret =
        agreement::agree_ephemeral(server_private, &client_pub, |shared| shared.to_vec())
            .map_err(|_| anyhow::anyhow!("X25519 agreement failed"))?;

    // Generate 6-digit PIN
    let pin = generate_pin(&rng);

    println!();
    println!("╔══════════════════════════════════════╗");
    println!("║  PAIRING PIN:  {pin}                ║");
    println!("║  Enter this PIN on your device       ║");
    println!("╚══════════════════════════════════════╝");
    println!();

    // Send PIN_CHALLENGE: SCREX_PIN(10) + server_pubkey(32)
    let mut response = Vec::with_capacity(10 + PUBKEY_LEN);
    response.extend_from_slice(MAGIC_PIN);
    response.extend_from_slice(server_public.as_ref());
    stream.write_all(&response)?;
    stream.flush()?;
    crate::vlog!("[pairing] sent PIN challenge to {addr} for device {device_id_hex}");

    // Store pending pairing for PIN verification
    {
        let ps = pairing.lock().unwrap();
        *ps.pending_pin.lock().unwrap() = Some(PendingPairing {
            pin: pin.clone(),
            device_id,
            ecdh_secret: ecdh_secret.clone(),
        });
    }
    crate::vlog!("[pairing] waiting for PIN answer from {addr} for device {device_id_hex}");

    // Wait for PIN answer: SCREX_ANSWER(12) + encrypted_pin_data
    let mut answer_header = [0u8; 12];
    stream.read_exact(&mut answer_header)?;
    if answer_header[..MAGIC_ANSWER.len()] != *MAGIC_ANSWER {
        anyhow::bail!("expected SCREX_ANSWER, got something else");
    }

    // Read encrypted PIN: nonce(12) + ciphertext(PIN_DIGITS) + tag(16) = 34 bytes
    let enc_len = 12 + PIN_DIGITS + crypto::TAG_LEN;
    let mut encrypted_pin = vec![0u8; enc_len];
    stream.read_exact(&mut encrypted_pin)?;

    // Derive encryption key from ECDH secret for the PIN exchange
    let pin_key = crypto::hkdf_sha256(&ecdh_secret, b"screx-pin-exchange", b"pin-encrypt");
    let cipher = crypto::SessionCipher::new(&pin_key);

    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&encrypted_pin[..12]);
    let mut ct = encrypted_pin[12..].to_vec();
    let plaintext = cipher.decrypt(&nonce, b"screx-pin-verify", &mut ct);
    crate::vlog!("[pairing] received PIN answer from {addr} for device {device_id_hex}");

    match plaintext {
        Some(pin_bytes) => {
            let received_pin = std::str::from_utf8(pin_bytes).unwrap_or("");
            if received_pin != pin {
                // Wrong PIN
                println!("[pairing] wrong PIN from device {device_id_hex}");
                let mut reject = Vec::new();
                reject.extend_from_slice(MAGIC_REJECT);
                let _ = stream.write_all(&reject);
                anyhow::bail!("wrong PIN");
            }
        }
        None => {
            println!("[pairing] PIN decryption failed for device {device_id_hex}");
            let mut reject = Vec::new();
            reject.extend_from_slice(MAGIC_REJECT);
            let _ = stream.write_all(&reject);
            anyhow::bail!("PIN decryption failed");
        }
    }

    // PIN correct — derive pairing key and session key
    let mut ikm = ecdh_secret.clone();
    ikm.extend_from_slice(pin.as_bytes());
    let pairing_key = crypto::hkdf_sha256(&ikm, b"screx-pairing-salt", b"screx-pairing");

    let session_salt: [u8; 32] = rand_bytes(&rng);
    let session_key = crypto::hkdf_sha256(&pairing_key, &session_salt, b"screx-session");

    // Store the pairing
    let daemon_device_id = {
        let mut ps = pairing.lock().unwrap();
        ps.store_device(&device_id, &pairing_key, &format!("{}", addr.ip()));
        *ps.pending_pin.lock().unwrap() = None;
        ps.daemon_device_id()
    };

    println!("[pairing] device {device_id_hex} paired successfully");

    // Send OK: SCREX_OK(10) + daemon_device_id(16) + session_salt(32) + hmac(32)
    let verify_hmac = crypto::hmac_sha256(&session_key, b"server-verify");
    let mut ok_msg = Vec::with_capacity(10 + DEVICE_ID_LEN + 32 + HMAC_LEN);
    ok_msg.extend_from_slice(MAGIC_OK);
    ok_msg.extend_from_slice(&daemon_device_id);
    ok_msg.extend_from_slice(&session_salt);
    ok_msg.extend_from_slice(&verify_hmac);
    stream.write_all(&ok_msg)?;
    stream.flush()?;
    crate::vlog!("[pairing] sent pairing OK to {addr} for device {device_id_hex}");

    let session = SessionInfo {
        session_id: 0,
        local_ip: None,
        session_key,
        client_addr: addr,
    };

    Ok(session)
}

fn handle_hello_request(
    stream: &mut TcpStream,
    addr: std::net::SocketAddr,
    header_buf: &[u8; 12],
    pairing: &Arc<Mutex<PairingState>>,
) -> Result<Option<SessionInfo>> {
    // SCREX_HELLO is 11 bytes, 1 byte of device_id in header_buf[11]
    // Read remaining: device_id(15) + client_nonce(32) = 47 bytes
    let mut rest = [0u8; 47];
    stream.read_exact(&mut rest)?;

    let mut device_id = [0u8; DEVICE_ID_LEN];
    device_id[0] = header_buf[11];
    device_id[1..].copy_from_slice(&rest[..15]);

    let mut client_nonce = [0u8; NONCE_LEN];
    client_nonce.copy_from_slice(&rest[15..47]);

    let device_id_hex = hex_encode(&device_id);
    println!("[pairing] hello from device {device_id_hex}");

    let ps = pairing.lock().unwrap();
    let Some(pairing_key) = ps.get_pairing_key(&device_id_hex) else {
        drop(ps);
        println!("[pairing] hello from unknown/unpaired device {device_id_hex}, sending REJECT");
        let _ = stream.write_all(MAGIC_REJECT);
        let _ = stream.flush();
        return Ok(None);
    };
    let daemon_device_id = ps.daemon_device_id();
    drop(ps);

    // Generate server nonce
    let rng = SystemRandom::new();
    let server_nonce: [u8; NONCE_LEN] = rand_bytes(&rng);

    // Derive session key
    let mut salt = Vec::with_capacity(NONCE_LEN * 2);
    salt.extend_from_slice(&client_nonce);
    salt.extend_from_slice(&server_nonce);
    let session_key = crypto::hkdf_sha256(&pairing_key, &salt, b"screx-session");

    let verify_hmac = crypto::hmac_sha256(&session_key, b"server-verify");

    // Send OK: SCREX_OK(10) + daemon_device_id(16) + server_nonce(32) + hmac(32)
    let mut ok_msg = Vec::with_capacity(10 + DEVICE_ID_LEN + NONCE_LEN + HMAC_LEN);
    ok_msg.extend_from_slice(MAGIC_OK);
    ok_msg.extend_from_slice(&daemon_device_id);
    ok_msg.extend_from_slice(&server_nonce);
    ok_msg.extend_from_slice(&verify_hmac);
    stream.write_all(&ok_msg)?;
    stream.flush()?;
    crate::vlog!("[pairing] sent hello OK to {addr} for device {device_id_hex}");

    let session = SessionInfo {
        session_id: 0,
        local_ip: None,
        session_key,
        client_addr: addr,
    };

    println!("[pairing] session established with paired device {device_id_hex}");
    Ok(Some(session))
}

fn run_control_loop(
    mut stream: TcpStream,
    shared: &Arc<crate::stream_server::SharedState>,
    stop: &Arc<AtomicBool>,
    session_key: [u8; 32],
    session_id: u64,
) -> Result<()> {
    stream.set_read_timeout(Some(CONTROL_READ_TIMEOUT)).ok();
    stream.set_nodelay(true).ok();

    let cipher = crypto::SessionCipher::new(&session_key);
    let mut len_buf = [0u8; 4];
    let mut msg_buf = vec![0u8; 256];
    let mut seq_expected = 0u32;
    let mut seq_initialized = false;
    let mut send_seq = 0u32;

    println!("[control] network control channel active");

    if let Some(hostname) = hostname() {
        let mut payload = Vec::with_capacity(CONTROL_HOSTNAME_MAGIC.len() + hostname.len());
        payload.extend_from_slice(CONTROL_HOSTNAME_MAGIC);
        payload.extend_from_slice(hostname.as_bytes());
        if let Err(e) = send_control_frame(&mut stream, &cipher, &mut send_seq, &payload) {
            eprintln!("[control] failed to send hostname: {e:#}");
        }
    }

    while !stop.load(Ordering::Relaxed) {
        if !shared.is_current_network_session(session_id) {
            println!("[control] network control loop exiting for stale session {session_id}");
            break;
        }

        match stream.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => {
                println!("[control] network control channel disconnected");
                break;
            }
        }

        let msg_len = u32::from_be_bytes(len_buf) as usize;
        if msg_len < 4 + crypto::TAG_LEN || msg_len > CONTROL_MAX_FRAME {
            eprintln!(
                "[control] invalid framed control message length: {msg_len}, closing channel"
            );
            break;
        }

        if msg_buf.len() < msg_len {
            msg_buf.resize(msg_len, 0);
        }

        match stream.read_exact(&mut msg_buf[..msg_len]) {
            Ok(()) => {}
            Err(_) => {
                println!("[control] network control channel disconnected (payload)");
                break;
            }
        }

        let seq_num = u32::from_be_bytes([msg_buf[0], msg_buf[1], msg_buf[2], msg_buf[3]]);
        if seq_initialized && seq_num != seq_expected {
            crate::vlog!(
                "[control] tcp control sequence mismatch: got={seq_num} expected={seq_expected}"
            );
        }
        seq_expected = seq_num.wrapping_add(1);
        seq_initialized = true;

        let nonce = crypto::nonce_control_client(seq_num);
        let aad = [msg_buf[0], msg_buf[1], msg_buf[2], msg_buf[3]];
        let plaintext = match cipher.decrypt(&nonce, &aad, &mut msg_buf[4..msg_len]) {
            Some(pt) => pt,
            None => {
                crate::vlog!("[control] failed to decrypt tcp control frame seq={seq_num}");
                continue;
            }
        };

        if plaintext == CONTROL_DISCONNECT_MAGIC {
            println!("[control] graceful disconnect requested by client");
            break;
        }

        crate::stream_server::handle_control_message_data(shared, plaintext);
    }

    crate::stream_server::drop_network_client(shared, session_id);

    Ok(())
}

fn send_control_frame(
    stream: &mut TcpStream,
    cipher: &crypto::SessionCipher,
    send_seq: &mut u32,
    payload: &[u8],
) -> Result<()> {
    let seq = *send_seq;
    *send_seq = send_seq.wrapping_add(1);

    let aad = seq.to_be_bytes();
    let nonce = crypto::nonce_control_server(seq);
    let mut encrypted = vec![0u8; payload.len() + crypto::TAG_LEN];
    encrypted[..payload.len()].copy_from_slice(payload);
    let encrypted_len = cipher.encrypt_slice(&nonce, &aad, &mut encrypted, payload.len());

    let body_len = (aad.len() + encrypted_len) as u32;
    stream.write_all(&body_len.to_be_bytes())?;
    stream.write_all(&aad)?;
    stream.write_all(&encrypted[..encrypted_len])?;
    stream.flush()?;
    Ok(())
}

fn generate_pin(rng: &SystemRandom) -> String {
    let mut buf = [0u8; 4];
    ring::rand::SecureRandom::fill(rng, &mut buf).expect("rng fill");
    let num = u32::from_be_bytes(buf) % 1_000_000;
    format!("{num:06}")
}

fn rand_bytes<const N: usize>(rng: &SystemRandom) -> [u8; N] {
    let mut buf = [0u8; N];
    ring::rand::SecureRandom::fill(rng, &mut buf).expect("rng fill");
    buf
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

fn chrono_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

pub fn run_unpair(device_id: Option<&str>) -> Result<()> {
    let mut state = PairingState::load();

    match device_id {
        Some("--all") => {
            let count = state.remove_all();
            println!("Removed all {count} paired device(s).");
        }
        Some(id) => {
            if state.remove_device(id) {
                println!("Unpaired device {id}.");
            } else {
                println!("Device {id} not found in paired devices.");
                let devices = state.list_devices();
                if !devices.is_empty() {
                    println!("\nCurrently paired devices:");
                    for (id, name, at) in &devices {
                        println!("  {id}  ({name}, paired at {at})");
                    }
                }
            }
        }
        None => {
            let devices = state.list_devices();
            if devices.is_empty() {
                println!("No paired devices.");
            } else {
                println!("Paired devices:");
                for (id, name, at) in &devices {
                    println!("  {id}  ({name}, paired at {at})");
                }
                println!("\nUsage: screx unpair <device_id> or screx unpair --all");
            }
        }
    }

    Ok(())
}
