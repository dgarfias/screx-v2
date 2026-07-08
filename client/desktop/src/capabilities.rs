// CAPS/STNG capability-negotiation wire format for the desktop client.
//
// This is an independent, from-scratch reimplementation of the TLV format
// documented in `docs/ARCHITECTURE.md` ("Capability Negotiation") — per this
// project's existing convention, there is no shared protocol crate between
// the daemon, the iPad client, and this desktop client, so each implements
// the wire format separately. Keep this file byte-for-byte compatible with
// the spec if the spec changes.
//
// Wire format (both directions), all multi-byte integers big-endian:
//   magic(4 bytes ASCII) + version(u8) + entry_count(u8) + entries...
//   each entry: tag(u8) + length(u16 BE) + value(length bytes)
//
// A parser that doesn't recognize a tag reads its length and skips that many
// bytes, then continues — unknown tags never cause a hard parse failure.

use serde::{Deserialize, Serialize};

pub const MAGIC_CAPS: &[u8] = b"CAPS";
pub const MAGIC_STNG: &[u8] = b"STNG";

// `parse_caps` deliberately ignores the version byte (TLV self-description
// makes it forward-compatible without a version check) but keeps this
// constant, exercised by the tests below, as living documentation of the
// wire format's current version.
#[allow(dead_code)]
const CAPS_VERSION: u8 = 1;
const STNG_VERSION: u8 = 1;

// CAPS tags (daemon -> client).
const TAG_CAMERA: u8 = 0x01;
const TAG_MICROPHONE: u8 = 0x02;
const TAG_SPEAKER: u8 = 0x03;
const TAG_GAMEPAD: u8 = 0x04;
const TAG_CODECS: u8 = 0x05;
const TAG_MAX_RESOLUTION: u8 = 0x06;
const TAG_MAX_FRAMERATE: u8 = 0x07;
const TAG_BITRATE_RANGE: u8 = 0x08;

// STNG tags (client -> daemon).
const TAG_RESOLUTION: u8 = 0x01;
const TAG_FRAMERATE: u8 = 0x02;
const TAG_CODEC: u8 = 0x03;
const TAG_BITRATE: u8 = 0x04;

pub const CODEC_H264: u8 = 0x00;
pub const CODEC_H265: u8 = 0x01;

/// Validation/clamping floors — see the plan's "Validation/clamping bounds"
/// section. Ceilings come from the daemon's advertised `DaemonCapabilities`.
pub const MIN_WIDTH: u32 = 640;
pub const MIN_HEIGHT: u32 = 360;
pub const MIN_FPS: u32 = 15;
pub const MIN_BITRATE_BPS: u32 = 500_000;

/// What the daemon told us it can do. Also used, via `Default`, as the
/// permissive fallback for a legacy daemon that never sends `CAPS` at all
/// (see the 2s timeout in `backend.rs`), and per-tag when a `CAPS` message
/// arrives but omits a tag.
#[derive(Clone, Debug, PartialEq)]
pub struct DaemonCapabilities {
    pub camera: bool,
    pub microphone: bool,
    pub speaker: bool,
    /// `None` = unsupported, `Some(n)` = max simultaneous controllers.
    /// Unused by this client today (no gamepad UI), kept for parity with the
    /// daemon's struct and forward compatibility.
    pub gamepad_max: Option<u8>,
    pub codecs: Vec<u8>,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
    pub bitrate_min_bps: u32,
    pub bitrate_max_bps: u32,
}

impl Default for DaemonCapabilities {
    fn default() -> Self {
        Self {
            camera: true,
            microphone: true,
            speaker: true,
            gamepad_max: None,
            codecs: vec![CODEC_H264, CODEC_H265],
            max_width: 3840,
            max_height: 2160,
            max_fps: 60,
            bitrate_min_bps: MIN_BITRATE_BPS,
            bitrate_max_bps: 20_000_000,
        }
    }
}

/// Parse a `CAPS` payload (including the leading `"CAPS"` magic) into
/// `DaemonCapabilities`. Missing tags fall back to the permissive defaults
/// above; unrecognized tags are skipped via their declared length rather
/// than causing a parse failure, per the wire-format spec.
pub fn parse_caps(payload: &[u8]) -> Option<DaemonCapabilities> {
    if !payload.starts_with(MAGIC_CAPS) {
        return None;
    }
    let mut pos = MAGIC_CAPS.len();
    if payload.len() < pos + 2 {
        return None;
    }
    let _version = payload[pos];
    pos += 1;
    let entry_count = payload[pos];
    pos += 1;

    let mut caps = DaemonCapabilities::default();

    for _ in 0..entry_count {
        if pos + 3 > payload.len() {
            // Truncated frame — stop parsing, return best-effort result
            // rather than failing the whole message.
            break;
        }
        let tag = payload[pos];
        let len = u16::from_be_bytes([payload[pos + 1], payload[pos + 2]]) as usize;
        pos += 3;
        if pos + len > payload.len() {
            break;
        }
        let value = &payload[pos..pos + len];
        match tag {
            TAG_CAMERA => {
                if let Some(&b) = value.first() {
                    caps.camera = b != 0;
                }
            }
            TAG_MICROPHONE => {
                if let Some(&b) = value.first() {
                    caps.microphone = b != 0;
                }
            }
            TAG_SPEAKER => {
                if let Some(&b) = value.first() {
                    caps.speaker = b != 0;
                }
            }
            TAG_GAMEPAD => {
                if value.len() >= 2 {
                    caps.gamepad_max = if value[0] != 0 { Some(value[1]) } else { None };
                }
            }
            TAG_CODECS => {
                if let Some(&count) = value.first() {
                    let avail = &value[1..];
                    let n = (count as usize).min(avail.len());
                    caps.codecs = avail[..n].to_vec();
                }
            }
            TAG_MAX_RESOLUTION => {
                if value.len() >= 4 {
                    caps.max_width = u16::from_be_bytes([value[0], value[1]]) as u32;
                    caps.max_height = u16::from_be_bytes([value[2], value[3]]) as u32;
                }
            }
            TAG_MAX_FRAMERATE => {
                if let Some(&fps) = value.first() {
                    caps.max_fps = fps as u32;
                }
            }
            TAG_BITRATE_RANGE => {
                if value.len() >= 8 {
                    caps.bitrate_min_bps =
                        u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
                    caps.bitrate_max_bps =
                        u32::from_be_bytes([value[4], value[5], value[6], value[7]]);
                }
            }
            _ => {
                // Unknown tag (future v2): skip via its declared length.
            }
        }
        pos += len;
    }

    Some(caps)
}

/// Stream settings the user wants to request, parsed from persisted UI
/// choices. `None` on any field means "omit this tag — use the daemon's own
/// default," per the `STNG` spec.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RequestedSettings {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    pub codec: Option<u8>,
    pub bitrate_bps: Option<u32>,
}

/// Clamp a client's requested settings against what the daemon actually
/// advertised. Mirrors the daemon-side clamping rules in the plan so the
/// client never sends a value the daemon would have had to clamp anyway.
/// A codec the daemon didn't advertise is dropped entirely (omitted, so the
/// daemon picks its own default) rather than sent and rejected.
pub fn clamp_settings(requested: &RequestedSettings, caps: &DaemonCapabilities) -> RequestedSettings {
    let mut out = RequestedSettings::default();

    if let (Some(w), Some(h)) = (requested.width, requested.height) {
        let max_w = caps.max_width.max(MIN_WIDTH);
        let max_h = caps.max_height.max(MIN_HEIGHT);
        out.width = Some(w.clamp(MIN_WIDTH, max_w));
        out.height = Some(h.clamp(MIN_HEIGHT, max_h));
    }

    if let Some(fps) = requested.fps {
        let max_fps = caps.max_fps.max(MIN_FPS);
        out.fps = Some(fps.clamp(MIN_FPS, max_fps));
    }

    if let Some(codec) = requested.codec {
        if caps.codecs.contains(&codec) {
            out.codec = Some(codec);
        }
    }

    if let Some(bps) = requested.bitrate_bps {
        let lo = caps.bitrate_min_bps.max(MIN_BITRATE_BPS);
        let hi = caps.bitrate_max_bps.max(lo);
        out.bitrate_bps = Some(bps.clamp(lo, hi));
    }

    out
}

/// Build an `STNG` message containing only the entries the caller actually
/// set. `entry_count = 0` (all fields `None`) is still a valid, meaningful
/// message ("everything default, just connect").
pub fn build_stng(settings: &RequestedSettings) -> Vec<u8> {
    let mut entries: Vec<(u8, Vec<u8>)> = Vec::new();

    if let (Some(w), Some(h)) = (settings.width, settings.height) {
        let mut value = Vec::with_capacity(4);
        value.extend_from_slice(&(w as u16).to_be_bytes());
        value.extend_from_slice(&(h as u16).to_be_bytes());
        entries.push((TAG_RESOLUTION, value));
    }
    if let Some(fps) = settings.fps {
        entries.push((TAG_FRAMERATE, vec![fps as u8]));
    }
    if let Some(codec) = settings.codec {
        entries.push((TAG_CODEC, vec![codec]));
    }
    if let Some(bps) = settings.bitrate_bps {
        entries.push((TAG_BITRATE, bps.to_be_bytes().to_vec()));
    }

    let mut out = Vec::with_capacity(
        MAGIC_STNG.len() + 2 + entries.iter().map(|(_, v)| 3 + v.len()).sum::<usize>(),
    );
    out.extend_from_slice(MAGIC_STNG);
    out.push(STNG_VERSION);
    out.push(entries.len() as u8);
    for (tag, value) in entries {
        out.push(tag);
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(&value);
    }
    out
}

/// The user's persisted stream-settings picks, as chosen in the desktop
/// settings dialog (`Main.qml`). Stored as curated preset strings (rather
/// than raw numbers) so the QML dialog and the persisted JSON share one
/// vocabulary; `to_requested` is where presets become wire values.
/// `"default"` (or any unrecognized value) means "omit this field."
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamSettingsChoice {
    #[serde(default = "default_choice")]
    pub resolution: String,
    #[serde(default = "default_choice")]
    pub framerate: String,
    #[serde(default = "default_choice")]
    pub codec: String,
    #[serde(default = "default_choice")]
    pub bitrate: String,
}

fn default_choice() -> String {
    "default".to_string()
}

impl Default for StreamSettingsChoice {
    fn default() -> Self {
        Self {
            resolution: default_choice(),
            framerate: default_choice(),
            codec: default_choice(),
            bitrate: default_choice(),
        }
    }
}

impl StreamSettingsChoice {
    pub fn to_requested(&self) -> RequestedSettings {
        let mut out = RequestedSettings::default();

        match self.resolution.as_str() {
            "1280x720" => {
                out.width = Some(1280);
                out.height = Some(720);
            }
            "1920x1080" => {
                out.width = Some(1920);
                out.height = Some(1080);
            }
            "2560x1440" => {
                out.width = Some(2560);
                out.height = Some(1440);
            }
            "3840x2160" => {
                out.width = Some(3840);
                out.height = Some(2160);
            }
            _ => {}
        }

        out.fps = match self.framerate.as_str() {
            "30" => Some(30),
            "60" => Some(60),
            _ => None,
        };

        out.codec = match self.codec.as_str() {
            "h264" => Some(CODEC_H264),
            "h265" => Some(CODEC_H265),
            _ => None,
        };

        out.bitrate_bps = match self.bitrate.as_str() {
            "3000000" => Some(3_000_000),
            "8000000" => Some(8_000_000),
            "15000000" => Some(15_000_000),
            _ => None,
        };

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_caps_missing_tags_use_defaults() {
        let mut payload = Vec::new();
        payload.extend_from_slice(MAGIC_CAPS);
        payload.push(CAPS_VERSION);
        payload.push(0); // entry_count = 0
        let caps = parse_caps(&payload).expect("should parse");
        assert_eq!(caps, DaemonCapabilities::default());
    }

    #[test]
    fn parse_caps_skips_unknown_tag() {
        let mut payload = Vec::new();
        payload.extend_from_slice(MAGIC_CAPS);
        payload.push(CAPS_VERSION);
        payload.push(2); // entry_count
        // Unknown tag 0xFE, length 3, garbage value.
        payload.push(0xFE);
        payload.extend_from_slice(&3u16.to_be_bytes());
        payload.extend_from_slice(&[1, 2, 3]);
        // Known tag: CAMERA = false.
        payload.push(TAG_CAMERA);
        payload.extend_from_slice(&1u16.to_be_bytes());
        payload.push(0);

        let caps = parse_caps(&payload).expect("should parse despite unknown tag");
        assert!(!caps.camera);
        assert!(caps.microphone); // still default
    }

    #[test]
    fn build_stng_empty_is_valid() {
        let stng = build_stng(&RequestedSettings::default());
        assert_eq!(&stng[..4], MAGIC_STNG);
        assert_eq!(stng[5], 0); // entry_count
    }

    #[test]
    fn clamp_settings_respects_bounds() {
        let caps = DaemonCapabilities {
            max_width: 1920,
            max_height: 1080,
            max_fps: 30,
            bitrate_min_bps: 1_000_000,
            bitrate_max_bps: 5_000_000,
            codecs: vec![CODEC_H264],
            ..DaemonCapabilities::default()
        };
        let requested = RequestedSettings {
            width: Some(3840),
            height: Some(2160),
            fps: Some(60),
            codec: Some(CODEC_H265),
            bitrate_bps: Some(20_000_000),
        };
        let clamped = clamp_settings(&requested, &caps);
        assert_eq!(clamped.width, Some(1920));
        assert_eq!(clamped.height, Some(1080));
        assert_eq!(clamped.fps, Some(30));
        assert_eq!(clamped.codec, None); // H.265 not advertised -> omitted
        assert_eq!(clamped.bitrate_bps, Some(5_000_000));
    }
}
