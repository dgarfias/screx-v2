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

/// Permissive fallback for the daemon's minimum advertised bitrate, used by
/// `DaemonCapabilities::default()` when a `CAPS` message omits the bitrate
/// range tag (or for a legacy daemon that never sends `CAPS` at all).
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

/// Format a bits-per-second value as a trimmed Mbps string for user-facing
/// messages, e.g. `20_000_000` -> `"20"`, `12_500_000` -> `"12.5"`.
fn fmt_mbps(bps: u32) -> String {
    let mbps = bps as f64 / 1_000_000.0;
    let formatted = format!("{mbps:.2}");
    formatted
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

/// Check a client's requested settings against what the daemon actually
/// advertised. Per product decision, the client never clamps or silently
/// drops an out-of-range setting — instead this returns a human-readable
/// description of every violation found (empty if `requested` fits entirely
/// within `caps`). The caller fails the connection and surfaces these
/// messages verbatim (joined) to the user rather than sending an `STNG` the
/// daemon would have to reject or clamp itself.
///
/// Only `Some` fields are checked; `None` means "daemon default," which is
/// always acceptable.
pub fn validate_settings(requested: &RequestedSettings, caps: &DaemonCapabilities) -> Vec<String> {
    let mut violations = Vec::new();

    if let (Some(w), Some(h)) = (requested.width, requested.height) {
        if w > caps.max_width || h > caps.max_height {
            violations.push(format!(
                "resolution is higher than maximum allowed by daemon (current: {w}\u{d7}{h}, maximum allowed: {}\u{d7}{})",
                caps.max_width, caps.max_height
            ));
        }
    }

    if let Some(fps) = requested.fps {
        if fps > caps.max_fps {
            violations.push(format!(
                "framerate is higher than maximum allowed by daemon (current: {fps} fps, maximum allowed: {} fps)",
                caps.max_fps
            ));
        }
    }

    if let Some(codec) = requested.codec {
        if !caps.codecs.contains(&codec) {
            let name = match codec {
                CODEC_H264 => "H.264".to_string(),
                CODEC_H265 => "H.265".to_string(),
                other => format!("0x{other:02x}"),
            };
            violations.push(format!("codec {name} is not supported by the daemon"));
        }
    }

    if let Some(bps) = requested.bitrate_bps {
        if bps > caps.bitrate_max_bps {
            violations.push(format!(
                "bitrate is higher than maximum allowed by daemon (current: {} Mbps, maximum allowed: {} Mbps)",
                fmt_mbps(bps),
                fmt_mbps(caps.bitrate_max_bps)
            ));
        } else if bps < caps.bitrate_min_bps {
            violations.push(format!(
                "bitrate is lower than minimum allowed by daemon (current: {} Mbps, minimum allowed: {} Mbps)",
                fmt_mbps(bps),
                fmt_mbps(caps.bitrate_min_bps)
            ));
        }
    }

    violations
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

        if let Some((w, h)) = parse_resolution(&self.resolution) {
            out.width = Some(w);
            out.height = Some(h);
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

        out.bitrate_bps = self.bitrate.parse::<u32>().ok().filter(|b| *b > 0);

        out
    }
}

/// Parse a "WxH" resolution string (e.g. "1920x1080") into `(width, height)`.
/// Both dimensions must be positive integers that fit in `u16` (plenty for
/// any real resolution). Anything else (missing 'x', non-numeric parts,
/// zero, out-of-range) yields `None`, which `to_requested` treats as "omit
/// this field."
fn parse_resolution(value: &str) -> Option<(u32, u32)> {
    let (w, h) = value.split_once('x')?;
    let w: u16 = w.parse().ok().filter(|v| *v > 0)?;
    let h: u16 = h.parse().ok().filter(|v| *v > 0)?;
    Some((w as u32, h as u32))
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

    fn test_caps() -> DaemonCapabilities {
        DaemonCapabilities {
            max_width: 1920,
            max_height: 1080,
            max_fps: 30,
            bitrate_min_bps: 1_000_000,
            bitrate_max_bps: 5_000_000,
            codecs: vec![CODEC_H264],
            ..DaemonCapabilities::default()
        }
    }

    #[test]
    fn validate_settings_in_range_is_empty() {
        let caps = test_caps();
        let requested = RequestedSettings {
            width: Some(1920),
            height: Some(1080),
            fps: Some(30),
            codec: Some(CODEC_H264),
            bitrate_bps: Some(5_000_000),
        };
        assert!(validate_settings(&requested, &caps).is_empty());
    }

    #[test]
    fn validate_settings_all_none_is_empty() {
        let caps = test_caps();
        assert!(validate_settings(&RequestedSettings::default(), &caps).is_empty());
    }

    #[test]
    fn validate_settings_flags_resolution_too_high() {
        let caps = test_caps();
        let requested = RequestedSettings {
            width: Some(3840),
            height: Some(2160),
            ..Default::default()
        };
        let violations = validate_settings(&requested, &caps);
        assert_eq!(violations.len(), 1);
        assert_eq!(
            violations[0],
            "resolution is higher than maximum allowed by daemon (current: 3840\u{d7}2160, maximum allowed: 1920\u{d7}1080)"
        );
    }

    #[test]
    fn validate_settings_flags_framerate_too_high() {
        let caps = test_caps();
        let requested = RequestedSettings {
            fps: Some(60),
            ..Default::default()
        };
        let violations = validate_settings(&requested, &caps);
        assert_eq!(violations.len(), 1);
        assert_eq!(
            violations[0],
            "framerate is higher than maximum allowed by daemon (current: 60 fps, maximum allowed: 30 fps)"
        );
    }

    #[test]
    fn validate_settings_flags_unsupported_codec() {
        let caps = test_caps();
        let requested = RequestedSettings {
            codec: Some(CODEC_H265),
            ..Default::default()
        };
        let violations = validate_settings(&requested, &caps);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0], "codec H.265 is not supported by the daemon");
    }

    #[test]
    fn validate_settings_flags_bitrate_too_high() {
        let caps = test_caps();
        let requested = RequestedSettings {
            bitrate_bps: Some(25_000_000),
            ..Default::default()
        };
        let violations = validate_settings(&requested, &caps);
        assert_eq!(violations.len(), 1);
        assert_eq!(
            violations[0],
            "bitrate is higher than maximum allowed by daemon (current: 25 Mbps, maximum allowed: 5 Mbps)"
        );
    }

    #[test]
    fn validate_settings_flags_bitrate_too_low() {
        let caps = test_caps();
        let requested = RequestedSettings {
            bitrate_bps: Some(200_000),
            ..Default::default()
        };
        let violations = validate_settings(&requested, &caps);
        assert_eq!(violations.len(), 1);
        assert_eq!(
            violations[0],
            "bitrate is lower than minimum allowed by daemon (current: 0.2 Mbps, minimum allowed: 1 Mbps)"
        );
    }

    #[test]
    fn validate_settings_reports_multiple_violations_together() {
        let caps = test_caps();
        let requested = RequestedSettings {
            width: Some(3840),
            height: Some(2160),
            fps: Some(60),
            codec: Some(CODEC_H265),
            bitrate_bps: Some(20_000_000),
        };
        let violations = validate_settings(&requested, &caps);
        assert_eq!(violations.len(), 4);
        assert!(violations[0].starts_with("resolution is higher"));
        assert!(violations[1].starts_with("framerate is higher"));
        assert!(violations[2].starts_with("codec H.265"));
        assert!(violations[3].starts_with("bitrate is higher"));
    }

    #[test]
    fn to_requested_accepts_non_preset_bitrate() {
        let choice = StreamSettingsChoice {
            bitrate: "12500000".to_string(),
            ..StreamSettingsChoice::default()
        };
        assert_eq!(choice.to_requested().bitrate_bps, Some(12_500_000));
    }

    #[test]
    fn to_requested_accepts_non_preset_resolution() {
        let choice = StreamSettingsChoice {
            resolution: "2360x1640".to_string(),
            ..StreamSettingsChoice::default()
        };
        let requested = choice.to_requested();
        assert_eq!(requested.width, Some(2360));
        assert_eq!(requested.height, Some(1640));
    }

    #[test]
    fn to_requested_rejects_malformed_resolution() {
        let choice = StreamSettingsChoice {
            resolution: "abcxdef".to_string(),
            ..StreamSettingsChoice::default()
        };
        let requested = choice.to_requested();
        assert_eq!(requested.width, None);
        assert_eq!(requested.height, None);
    }

    #[test]
    fn to_requested_rejects_non_numeric_bitrate() {
        let choice = StreamSettingsChoice {
            bitrate: "default".to_string(),
            ..StreamSettingsChoice::default()
        };
        assert_eq!(choice.to_requested().bitrate_bps, None);
    }

    #[test]
    fn to_requested_rejects_zero_bitrate() {
        let choice = StreamSettingsChoice {
            bitrate: "0".to_string(),
            ..StreamSettingsChoice::default()
        };
        assert_eq!(choice.to_requested().bitrate_bps, None);
    }
}
