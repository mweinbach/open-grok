/// Events emitted by [`crate::pipeline::run_voice_pipeline`] to the pager event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceEvent {
    /// Partial transcript while the user is speaking (`interim_results` / non-final chunks).
    InterimTranscript { text: String },

    /// Utterance complete (`speech_final` on streaming STT, or batch result).
    UtteranceFinal { text: String },

    /// Non-fatal or fatal error from STT.
    Error { message: String },
}
