//! Message type bytes for QUIC control/input streams and client -> daemon
//! QUIC datagrams. Every message is `type(u8) + body`.
//! See docs/ARCHITECTURE.md "Protocol Details" for the body layouts; the
//! iPad app mirrors these values in `Protocol.swift`.

/// Client -> daemon control/input stream messages.
pub mod client {
    /// Stream settings, sent once in reply to `daemon::CAPS`.
    pub const SETTINGS: u8 = 0x01;
    /// Keyframe request.
    pub const PLI: u8 = 0x02;
    /// Latency probe; answered with `daemon::PONG`.
    pub const PING: u8 = 0x03;
    /// Graceful end of session.
    pub const DISCONNECT: u8 = 0x04;
    pub const DISPLAY_PRESENTED: u8 = 0x05;
    pub const TOUCH: u8 = 0x10;
    pub const KEY: u8 = 0x11;
    pub const RAWKEY: u8 = 0x12;
    pub const MOUSE: u8 = 0x13;
    /// Physical mouse/keyboard attached or detached.
    pub const PERIPH: u8 = 0x14;
    pub const GAMEPAD: u8 = 0x15;
    /// Speaker forwarding on/off: `enabled(u8)`.
    pub const SPEAKER: u8 = 0x20;
    /// Virtual microphone on/off: `enabled(u8)`.
    pub const MIC: u8 = 0x21;
    /// Virtual webcam config: `width(u16) height(u16) fps(u16)`, all zero = off.
    pub const CAMERA: u8 = 0x22;
}

/// Daemon -> client control stream messages.
pub mod daemon {
    /// Capabilities + host name, sent once when the control channel opens.
    pub const CAPS: u8 = 0x80;
    /// Reply to `client::PING`.
    pub const PONG: u8 = 0x81;
    pub const DISPLAY_STATUS: u8 = 0x82;
}

/// Client -> daemon QUIC datagram payloads.
pub mod datagram {
    /// One chunk of a camera JPEG: `frame_id(u32) chunk_idx(u16) total_chunks(u16) data`.
    pub const CAMERA: u8 = 0x02;
    /// One mic Opus packet: `seq(u32) opus`.
    pub const MIC: u8 = 0x03;
    /// Speaker playout report, see `audio::AudioReport`.
    pub const AUDIO_REPORT: u8 = 0x04;
}

/// Fixed-layout field values shared by `CAPS` and `SETTINGS`.
pub const CODEC_H264: u8 = 0x00;
pub const CODEC_H265: u8 = 0x01;
/// `SETTINGS` codec value meaning "use the daemon's default codec".
pub const CODEC_DEFAULT: u8 = 0xFF;

/// Daemon-to-client media datagram header size.
pub const MEDIA_HEADER_LEN: usize = 22;
