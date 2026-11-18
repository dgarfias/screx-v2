//! Frame IDs and generations for the media header: the timelines the iPad's decoders and
//! jitter buffer follow.
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT_GENERATION: AtomicU32 = AtomicU32::new(1);
pub struct ServerPacketSequence {
    pub generation: u32,
    next_frame: Option<u32>,
}
impl ServerPacketSequence {
    pub fn new() -> Self {
        let generation = NEXT_GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .expect("media generation exhausted");
        Self {
            generation,
            next_frame: Some(0),
        }
    }
    pub fn take_frame(&mut self) -> anyhow::Result<u32> {
        let frame = self
            .next_frame
            .ok_or_else(|| anyhow::anyhow!("media frame IDs exhausted"))?;
        self.next_frame = frame.checked_add(1);
        Ok(frame)
    }
}
