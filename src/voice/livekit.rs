use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use fluxer::prelude::Context;
use livekit::Room;
use livekit::options::TrackPublishOptions;
use livekit::track::{LocalAudioTrack, LocalTrack, TrackSource};
use livekit::webrtc::audio_source::RtcAudioSource;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::prelude::{AudioFrame, AudioSourceOptions};

use super::{CHANNELS, SAMPLE_RATE, SAMPLES_PER_CHANNEL, VoiceTransport};

/// Outgoing buffer. Short, so a skip only loses this much latency even before
/// `clear_buffer`.
const QUEUE_MS: u32 = 200;
const TRACK_NAME: &str = "music";

pub struct LiveKitTransport {
    ctx: Context,
    guild_id: String,
    room: Arc<Room>,
    source: NativeAudioSource,
}

impl LiveKitTransport {
    /// Joins through fluxer-rust, then swaps the library's default track
    /// (whose source it keeps private) for one we can feed frames into.
    pub async fn join(ctx: &Context, guild_id: &str, channel_id: &str) -> Result<Self> {
        let conn = ctx.join_voice(guild_id, channel_id).await?;
        let room = conn.room.clone();
        let participant = room.local_participant();

        for sid in participant.track_publications().into_keys() {
            participant.unpublish_track(&sid).await?;
        }

        let source = NativeAudioSource::new(
            AudioSourceOptions::default(),
            SAMPLE_RATE,
            CHANNELS,
            QUEUE_MS,
        );
        let track = LocalAudioTrack::create_audio_track(
            TRACK_NAME,
            RtcAudioSource::Native(source.clone()),
        );
        participant
            .publish_track(
                LocalTrack::Audio(track),
                TrackPublishOptions { source: TrackSource::Microphone, ..Default::default() },
            )
            .await?;

        Ok(Self { ctx: ctx.clone(), guild_id: guild_id.to_string(), room, source })
    }
}

#[async_trait]
impl VoiceTransport for LiveKitTransport {
    async fn push_frame(&self, samples: &[i16]) -> Result<()> {
        let frame = AudioFrame {
            data: samples.into(),
            sample_rate: SAMPLE_RATE,
            num_channels: CHANNELS,
            samples_per_channel: SAMPLES_PER_CHANNEL,
        };
        self.source.capture_frame(&frame).await.map_err(|e| anyhow!("{e:?}"))
    }

    fn clear_buffer(&self) {
        self.source.clear_buffer();
    }

    async fn leave(&self) -> Result<()> {
        self.source.clear_buffer();
        // Close the room ourselves too: after a gateway reconnect fluxer-rust's
        // new Context no longer knows this room.
        let _ = self.room.close().await;
        self.ctx.leave_voice(&self.guild_id).await?;
        Ok(())
    }
}
