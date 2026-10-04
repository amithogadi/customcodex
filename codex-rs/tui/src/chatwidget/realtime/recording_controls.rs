//! Local bookkeeping for interrupted captions in historical realtime sessions.

use super::*;

impl ChatWidget {
    pub(super) fn release_realtime_speaker(&mut self) {
        let acknowledged = self
            .realtime_conversation
            .interruption_acknowledged_until
            .take()
            .is_some();
        self.realtime_conversation.speaker_suppression_generation = None;
        if acknowledged {}
    }

    pub(super) fn suppress_active_realtime_speaker(&mut self) {
        // Quiet turns must not wait for captions before accepting their first
        // audio packets. Only interrupt output that may belong to an older turn.
        if self
            .realtime_conversation
            .assistant_transcript_generation
            .is_some()
            || self
                .realtime_conversation
                .pending_speech
                .iter()
                .any(|pending| {
                    pending.input_generation != self.realtime_conversation.input_generation
                        && !pending.captioned
                        && matches!(
                            pending.state,
                            PendingSpeechState::Queued(_) | PendingSpeechState::Accepted
                        )
                })
            || self.realtime_conversation.speaker_level > 0
            || self
                .realtime_conversation
                .speaker_active_until
                .is_some_and(|deadline| deadline > Instant::now())
            || self
                .realtime_conversation
                .speaker_suppression_generation
                .is_some()
        {
            self.suppress_realtime_speaker();
        }
    }

    pub(super) fn suppress_realtime_speaker(&mut self) {
        self.realtime_conversation.speaker_suppression_generation =
            Some(self.realtime_conversation.input_generation);
        self.realtime_conversation.speaker_level = 0;
        self.realtime_conversation.speaker_intensity = 0;
        for (_, speaker) in &mut self.realtime_conversation.audio_meter_history {
            *speaker = 0;
        }
        self.realtime_conversation.speaker_active_until = None;
    }

    pub(super) fn resume_realtime_speaker_for(&mut self, role: &str, text: &str) {
        if role == "assistant"
            && !text.trim().is_empty()
            && self.realtime_conversation.assistant_transcript_generation
                == Some(self.realtime_conversation.input_generation)
            && self.realtime_conversation.latest_input_was_voice
            && self.realtime_conversation.speaker_suppression_generation
                == Some(self.realtime_conversation.input_generation)
        {
            self.release_realtime_speaker();
        }
    }
}
