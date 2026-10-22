//! Everything that talks to voice sits behind [`VoiceTransport`], so the LiveKit
//! backend can be swapped when Fluxer ships its own voice server.

mod livekit;

pub use self::livekit::LiveKitTransport;

use async_trait::async_trait;

/// 48 kHz stereo, 20 ms per frame.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
pub const SAMPLES_PER_CHANNEL: u32 = 960;
/// Interleaved i16 samples in one frame.
pub const FRAME_SAMPLES: usize = (SAMPLES_PER_CHANNEL * CHANNELS) as usize;

#[async_trait]
pub trait VoiceTransport: Send + Sync {
    /// Queues one frame of interleaved PCM. Waits while the outgoing buffer is
    /// full, which paces the caller to real time.
    async fn push_frame(&self, samples: &[i16]) -> anyhow::Result<()>;

    /// Drops audio that is buffered but not yet sent, so skips are instant.
    fn clear_buffer(&self);

    async fn leave(&self) -> anyhow::Result<()>;
}
