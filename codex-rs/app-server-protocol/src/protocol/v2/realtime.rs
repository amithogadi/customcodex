use crate::JsonSchema;
use crate::TS;
use codex_protocol::protocol::RealtimeAudioFrame as CoreRealtimeAudioFrame;
use codex_protocol::protocol::RealtimeConversationVersion;
use codex_protocol::realtime::BemItemPresentation as CoreBemItemPresentation;
use codex_protocol::realtime::RealtimeItem as CoreRealtimeItem;
use codex_protocol::realtime::RealtimeItemContent as CoreRealtimeItemContent;
use codex_protocol::realtime::RealtimeSessionOutcome as CoreRealtimeSessionOutcome;
use codex_protocol::realtime::RealtimeTranscriptRole as CoreRealtimeTranscriptRole;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

/// EXPERIMENTAL - a thread-scoped realtime item in the canonical timeline.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadRealtimeItem {
    pub id: String,
    pub realtime_session_id: String,
    #[serde(flatten)]
    pub content: ThreadRealtimeItemContent,
}

/// EXPERIMENTAL - durable facts describing realtime speech and promoted work.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[ts(tag = "type", rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadRealtimeItemContent {
    RealtimeSessionStarted,
    TranscriptSegment {
        role: ThreadRealtimeTranscriptRole,
        text: String,
    },
    BemItemPromoted {
        turn_id: String,
        item_id: String,
        presentation: ThreadRealtimeBemItemPresentation,
    },
    RealtimeSessionClosed {
        outcome: ThreadRealtimeSessionOutcome,
    },
}

/// EXPERIMENTAL - how an existing agent item appears in a realtime conversation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[ts(tag = "type", rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadRealtimeBemItemPresentation {
    WholeItem,
    InlineMarkdown,
    InlineVisualization { index: u32 },
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadRealtimeTranscriptRole {
    User,
    Assistant,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum ThreadRealtimeSessionOutcome {
    Ended,
    Failed,
}

impl From<CoreRealtimeItem> for ThreadRealtimeItem {
    fn from(item: CoreRealtimeItem) -> Self {
        let CoreRealtimeItem {
            id,
            realtime_session_id,
            content,
        } = item;
        let content = match content {
            CoreRealtimeItemContent::RealtimeSessionStarted => {
                ThreadRealtimeItemContent::RealtimeSessionStarted
            }
            CoreRealtimeItemContent::TranscriptSegment { role, text } => {
                ThreadRealtimeItemContent::TranscriptSegment {
                    role: match role {
                        CoreRealtimeTranscriptRole::User => ThreadRealtimeTranscriptRole::User,
                        CoreRealtimeTranscriptRole::Assistant => {
                            ThreadRealtimeTranscriptRole::Assistant
                        }
                    },
                    text,
                }
            }
            CoreRealtimeItemContent::BemItemPromoted {
                turn_id,
                item_id,
                presentation,
            } => ThreadRealtimeItemContent::BemItemPromoted {
                turn_id,
                item_id,
                presentation: match presentation {
                    CoreBemItemPresentation::WholeItem => {
                        ThreadRealtimeBemItemPresentation::WholeItem
                    }
                    CoreBemItemPresentation::InlineMarkdown => {
                        ThreadRealtimeBemItemPresentation::InlineMarkdown
                    }
                    CoreBemItemPresentation::InlineVisualization { index } => {
                        ThreadRealtimeBemItemPresentation::InlineVisualization { index }
                    }
                },
            },
            CoreRealtimeItemContent::RealtimeSessionClosed { outcome } => {
                ThreadRealtimeItemContent::RealtimeSessionClosed {
                    outcome: match outcome {
                        CoreRealtimeSessionOutcome::Ended => ThreadRealtimeSessionOutcome::Ended,
                        CoreRealtimeSessionOutcome::Failed => ThreadRealtimeSessionOutcome::Failed,
                    },
                }
            }
        };
        Self {
            id,
            realtime_session_id,
            content,
        }
    }
}

/// EXPERIMENTAL - thread realtime audio chunk.
#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadRealtimeAudioChunk {
    pub data: String,
    pub sample_rate: u32,
    pub num_channels: u16,
    pub samples_per_channel: Option<u32>,
    pub item_id: Option<String>,
}

impl From<CoreRealtimeAudioFrame> for ThreadRealtimeAudioChunk {
    fn from(value: CoreRealtimeAudioFrame) -> Self {
        let CoreRealtimeAudioFrame {
            data,
            sample_rate,
            num_channels,
            samples_per_channel,
            item_id,
        } = value;
        Self {
            data,
            sample_rate,
            num_channels,
            samples_per_channel,
            item_id,
        }
    }
}

impl From<ThreadRealtimeAudioChunk> for CoreRealtimeAudioFrame {
    fn from(value: ThreadRealtimeAudioChunk) -> Self {
        let ThreadRealtimeAudioChunk {
            data,
            sample_rate,
            num_channels,
            samples_per_channel,
            item_id,
        } = value;
        Self {
            data,
            sample_rate,
            num_channels,
            samples_per_channel,
            item_id,
        }
    }
}

/// EXPERIMENTAL - emitted when thread realtime startup is accepted.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadRealtimeStartedNotification {
    pub thread_id: String,
    pub realtime_session_id: Option<String>,
    pub version: RealtimeConversationVersion,
}

/// EXPERIMENTAL - raw non-audio thread realtime item emitted by the backend.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadRealtimeItemAddedNotification {
    pub thread_id: String,
    pub item: JsonValue,
}

/// EXPERIMENTAL - a realtime timeline item started before its content streams.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadRealtimeItemStartedNotification {
    pub thread_id: String,
    pub item: ThreadRealtimeItem,
}

/// EXPERIMENTAL - text appended to an active realtime transcript item.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadRealtimeItemTranscriptDeltaNotification {
    pub thread_id: String,
    pub item_id: String,
    pub delta: String,
}

/// EXPERIMENTAL - a realtime timeline item published after canonical commit.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadRealtimeItemCompletedNotification {
    pub thread_id: String,
    pub item: ThreadRealtimeItem,
}

/// EXPERIMENTAL - flat transcript delta emitted whenever realtime
/// transcript text changes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadRealtimeTranscriptDeltaNotification {
    pub thread_id: String,
    pub role: String,
    /// Live transcript delta from the realtime event.
    pub delta: String,
}

/// EXPERIMENTAL - final transcript text emitted when realtime completes
/// a transcript part.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadRealtimeTranscriptDoneNotification {
    pub thread_id: String,
    pub role: String,
    /// Final complete text for the transcript part.
    pub text: String,
}

/// EXPERIMENTAL - streamed output audio emitted by thread realtime.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadRealtimeOutputAudioDeltaNotification {
    pub thread_id: String,
    pub audio: ThreadRealtimeAudioChunk,
}

/// EXPERIMENTAL - emitted with the remote SDP for a WebRTC realtime session.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadRealtimeSdpNotification {
    pub thread_id: String,
    pub sdp: String,
}

/// EXPERIMENTAL - emitted when thread realtime encounters an error.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadRealtimeErrorNotification {
    pub thread_id: String,
    pub message: String,
}

/// EXPERIMENTAL - emitted when thread realtime transport closes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadRealtimeClosedNotification {
    pub thread_id: String,
    pub reason: Option<String>,
}
