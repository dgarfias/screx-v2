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

// Wire protocol magic bytes
const MAGIC_PAIR: &[u8] = b"SCREX_PAIR";      // 10 bytes
const MAGIC_HELLO: &[u8] = b"SCREX_HELLO";    // 11 bytes
const MAGIC_PIN: &[u8] = b"SCREX_PIN\0";      // 10 bytes (padded for alignment)
const MAGIC_ANSWER: &[u8] = b"SCREX_ANSWER";  // 12 bytes
const MAGIC_OK: &[u8] = b"SCREX_OK\0\0";      // 10 bytes
const MAGIC_REJECT: &[u8] = b"SCREX_REJECT";  // 12 bytes

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
    pub session_key: [u8; 32],
    pub client_addr: std::net::SocketAddr,
}

pub struct PairingState {
    paired_devices: HashMap<String, PairedDevice>, // device_id (hex) -> PairedDevice
    config_path: PathBuf,
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

        Self {
            paired_devices,
            config_path,
            pending_pin: Mutex::new(None),
        }
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

    fn store_device(&mut self, device_id: &[u8; DEVICE_ID_LEN], pairing_key: &[u8; 32], name: &str) {
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

fn config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("screx")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config").join("screx")
    } else {
        PathBuf::from("/tmp/screx")
    }
}

const MAGIC_BUSY: &[u8] = b"SCREX_BUSY\0\0"; // 12 bytes

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
                // Reject if a session is already active (single-client mode)
                if shared.has_active_client.load(Ordering::Relaxed) {
                    println!("[pairing] rejecting {addr} — session already active");
                    let _ = stream.write_all(MAGIC_BUSY);
                    let _ = stream.flush();
                    continue;
                }

                println!("[pairing] incoming handshake from {addr}");
                stream
                    .set_read_timeout(Some(Duration::from_secs(30)))
                    .ok();
                stream
                    .set_write_timeout(Some(Duration::from_secs(10)))
                    .ok();

                match handle_handshake(stream, addr, &pairing, &session_tx) {
                    Ok(()) => println!("[pairing] handshake with {addr} completed"),
                    Err(e) => eprintln!("[pairing] handshake with {addr} failed: {e:#}"),
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
    mut stream: TcpStream,
    addr: std::net::SocketAddr,
    pairing: &Arc<Mutex<PairingState>>,
    session_tx: &Arc<Mutex<Option<SessionInfo>>>,
) -> Result<()> {
    // Read the first message to determine if this is PAIR or HELLO
    let mut header = [0u8; 12]; // max magic length
    stream.read_exact(&mut header)?;

    if header[..MAGIC_PAIR.len()] == *MAGIC_PAIR {
        crate::vlog!("[pairing] handshake type=PAIR from {addr}");
        handle_pair_request(&mut stream, addr, &header, pairing, session_tx)
    } else if header[..MAGIC_HELLO.len()] == *MAGIC_HELLO {
        crate::vlog!("[pairing] handshake type=HELLO from {addr}");
        handle_hello_request(&mut stream, addr, &header, pairing, session_tx)
    } else {
        anyhow::bail!("unknown handshake magic");
    }
}

fn handle_pair_request(
    stream: &mut TcpStream,
    addr: std::net::SocketAddr,
    header_buf: &[u8; 12],
    pairing: &Arc<Mutex<PairingState>>,
    session_tx: &Arc<Mutex<Option<SessionInfo>>>,
) -> Result<()> {
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

    // Check if already paired — if so, treat like a HELLO with fresh ECDH
    {
        let ps = pairing.lock().unwrap();
        if ps.is_paired(&device_id_hex) {
            crate::vlog!("[pairing] device {device_id_hex} already paired, upgrading to session");
            drop(ps);
            return handle_pair_already_paired(stream, addr, &device_id, &client_pubkey, pairing, session_tx);
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
    let ecdh_secret = agreement::agree_ephemeral(server_private, &client_pub, |shared| {
        shared.to_vec()
    })
    .map_err(|_| anyhow::anyhow!("X25519 agreement failed"))?;

    // Generate 6-digit PIN
    let pin = generate_pin(&rng);

    println!();
    println!("╔══════════════════════════════════════╗");
    println!("║  PAIRING PIN:  {pin}                 ║");
    println!("║  Enter this PIN on your iPad         ║");
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
    {
        let mut ps = pairing.lock().unwrap();
        ps.store_device(&device_id, &pairing_key, &format!("{}", addr.ip()));
        *ps.pending_pin.lock().unwrap() = None;
    }

    println!("[pairing] device {device_id_hex} paired successfully");

    // Send OK: SCREX_OK(10) + session_salt(32) + hmac(32)
    let verify_hmac = crypto::hmac_sha256(&session_key, b"server-verify");
    let mut ok_msg = Vec::with_capacity(10 + 32 + HMAC_LEN);
    ok_msg.extend_from_slice(MAGIC_OK);
    ok_msg.extend_from_slice(&session_salt);
    ok_msg.extend_from_slice(&verify_hmac);
    stream.write_all(&ok_msg)?;
    stream.flush()?;
    crate::vlog!("[pairing] sent pairing OK to {addr} for device {device_id_hex}");

    // Publish session
    let replaced = session_tx.lock().unwrap().replace(SessionInfo {
        session_key,
        client_addr: addr,
    });
    crate::vlog!(
        "[pairing] published paired session for device {device_id_hex}: udp_ip={} replaced_previous_session={}",
        addr.ip(),
        replaced.is_some()
    );

    Ok(())
}

fn handle_pair_already_paired(
    stream: &mut TcpStream,
    addr: std::net::SocketAddr,
    device_id: &[u8; DEVICE_ID_LEN],
    client_pubkey: &[u8; PUBKEY_LEN],
    pairing: &Arc<Mutex<PairingState>>,
    session_tx: &Arc<Mutex<Option<SessionInfo>>>,
) -> Result<()> {
    let device_id_hex = hex_encode(device_id);
    let pairing_key = pairing
        .lock()
        .unwrap()
        .get_pairing_key(&device_id_hex)
        .context("paired device key not found")?;

    // Generate server ECDH keypair for forward secrecy
    let rng = SystemRandom::new();
    let server_private = EphemeralPrivateKey::generate(&X25519, &rng)
        .map_err(|e| anyhow::anyhow!("X25519 keygen: {e}"))?;
    let server_public = server_private
        .compute_public_key()
        .map_err(|e| anyhow::anyhow!("X25519 pubkey: {e}"))?;

    let client_pub = UnparsedPublicKey::new(&X25519, client_pubkey);
    let ecdh_secret = agreement::agree_ephemeral(server_private, &client_pub, |shared| {
        shared.to_vec()
    })
    .map_err(|_| anyhow::anyhow!("X25519 agreement failed"))?;

    // Derive session key from pairing_key + ECDH
    let mut ikm = Vec::new();
    ikm.extend_from_slice(&pairing_key);
    ikm.extend_from_slice(&ecdh_secret);
    let session_key = crypto::hkdf_sha256(&ikm, b"screx-reconnect-salt", b"screx-session");

    let verify_hmac = crypto::hmac_sha256(&session_key, b"server-verify");

    // Send OK: SCREX_OK(10) + server_pubkey(32) + hmac(32)
    let mut ok_msg = Vec::with_capacity(10 + PUBKEY_LEN + HMAC_LEN);
    ok_msg.extend_from_slice(MAGIC_OK);
    ok_msg.extend_from_slice(server_public.as_ref());
    ok_msg.extend_from_slice(&verify_hmac);
    stream.write_all(&ok_msg)?;
    stream.flush()?;
    crate::vlog!("[pairing] sent reconnect OK to {addr} for device {device_id_hex}");

    let replaced = session_tx.lock().unwrap().replace(SessionInfo {
        session_key,
        client_addr: addr,
    });
    crate::vlog!(
        "[pairing] published reconnect session for device {device_id_hex}: udp_ip={} replaced_previous_session={}",
        addr.ip(),
        replaced.is_some()
    );

    println!("[pairing] reconnected paired device {device_id_hex}");
    Ok(())
}

fn handle_hello_request(
    stream: &mut TcpStream,
    addr: std::net::SocketAddr,
    header_buf: &[u8; 12],
    pairing: &Arc<Mutex<PairingState>>,
    session_tx: &Arc<Mutex<Option<SessionInfo>>>,
) -> Result<()> {
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

    let pairing_key = pairing
        .lock()
        .unwrap()
        .get_pairing_key(&device_id_hex)
        .context("unknown device — not paired")?;

    // Generate server nonce
    let rng = SystemRandom::new();
    let server_nonce: [u8; NONCE_LEN] = rand_bytes(&rng);

    // Derive session key
    let mut salt = Vec::with_capacity(NONCE_LEN * 2);
    salt.extend_from_slice(&client_nonce);
    salt.extend_from_slice(&server_nonce);
    let session_key = crypto::hkdf_sha256(&pairing_key, &salt, b"screx-session");

    let verify_hmac = crypto::hmac_sha256(&session_key, b"server-verify");

    // Send OK: SCREX_OK(10) + server_nonce(32) + hmac(32)
    let mut ok_msg = Vec::with_capacity(10 + NONCE_LEN + HMAC_LEN);
    ok_msg.extend_from_slice(MAGIC_OK);
    ok_msg.extend_from_slice(&server_nonce);
    ok_msg.extend_from_slice(&verify_hmac);
    stream.write_all(&ok_msg)?;
    stream.flush()?;
    crate::vlog!("[pairing] sent hello OK to {addr} for device {device_id_hex}");

    let replaced = session_tx.lock().unwrap().replace(SessionInfo {
        session_key,
        client_addr: addr,
    });
    crate::vlog!(
        "[pairing] published hello session for device {device_id_hex}: udp_ip={} replaced_previous_session={}",
        addr.ip(),
        replaced.is_some()
    );

    println!("[pairing] session established with paired device {device_id_hex}");
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
