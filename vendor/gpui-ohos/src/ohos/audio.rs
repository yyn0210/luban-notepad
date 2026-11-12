//! Owned, reusable audio files for GPUI clients. Commands are delivered in
//! order, and every submitted descriptor remains live until native duplication.
use crate::ForegroundExecutor;
use futures::{channel::mpsc, StreamExt};
use openharmony_ability::OpenHarmonyApp;
pub use openharmony_ability_plugin_audio::AudioBridgePlugin;
use openharmony_ability_plugin_audio::{AudioClient, AudioUpdateRequest};
use std::{
    fs::File,
    os::fd::AsFd,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_PLAYER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct NativeAudioService {
    app: OpenHarmonyApp,
    executor: ForegroundExecutor,
}
#[derive(Clone, Copy, Default)]
pub struct NativeAudioPlayerOptions {
    pub diagnostics: bool,
}
impl NativeAudioService {
    /// Register AudioBridgePlugin and the matching ArkTS AudioPlugin before creating this service.
    pub fn new(
        app: OpenHarmonyApp,
        executor: ForegroundExecutor,
    ) -> Result<Self, NativeAudioError> {
        if app
            .registered_plugin::<AudioBridgePlugin>()
            .ok()
            .flatten()
            .is_none()
        {
            return Err(NativeAudioError::Unavailable);
        }
        Ok(Self { app, executor })
    }
    pub fn player(
        &self,
        _options: NativeAudioPlayerOptions,
    ) -> Result<NativeAudioPlayer, NativeAudioError> {
        let client = AudioClient::new(&self.app).map_err(|_| NativeAudioError::Unavailable)?;
        let id = NEXT_PLAYER
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| NativeAudioError::Unavailable)?
            .to_string();
        let (sender, mut receiver) = mpsc::unbounded::<Command>();
        let player_id = id.clone();
        self.executor
            .spawn(async move {
                while let Some(command) = receiver.next().await {
                    let request = AudioUpdateRequest {
                        player: player_id.clone(),
                        replace: command.replace,
                        release: command.release,
                        play: command.play,
                        volume: command.volume,
                        report_errors: command.report_errors,
                        label: command.label,
                    };
                    if let Err(error) = client
                        .update(request, command.source.as_ref().map(AsFd::as_fd))
                        .await
                    {
                        log::warn!("OHOS audio player {player_id}: {error}");
                    }
                    // The command, including its File, drops only after acknowledgement.
                }
            })
            .detach();
        Ok(NativeAudioPlayer { id, sender })
    }
}

struct Command {
    source: Option<File>,
    replace: bool,
    release: bool,
    play: bool,
    volume: f64,
    report_errors: bool,
    label: String,
}

pub struct NativeAudioPlayer {
    id: String,
    sender: mpsc::UnboundedSender<Command>,
}
impl NativeAudioPlayer {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn clear(&self) -> Result<(), NativeAudioError> {
        self.send(Command {
            source: None,
            replace: true,
            release: false,
            play: false,
            volume: 1.,
            report_errors: false,
            label: String::new(),
        })
    }
    pub fn replace_and_play(
        &self,
        source: File,
        volume: f64,
        report_errors: bool,
        label: String,
    ) -> Result<(), NativeAudioError> {
        self.send(Command {
            source: Some(source),
            replace: true,
            release: false,
            play: true,
            volume,
            report_errors,
            label,
        })
    }
    pub fn replay(&self, volume: f64) -> Result<(), NativeAudioError> {
        self.send(Command {
            source: None,
            replace: false,
            release: false,
            play: true,
            volume,
            report_errors: false,
            label: String::new(),
        })
    }
    fn send(&self, command: Command) -> Result<(), NativeAudioError> {
        if !command.volume.is_finite() || !(0.0..=1.0).contains(&command.volume) {
            return Err(NativeAudioError::InvalidVolume);
        }
        self.sender
            .unbounded_send(command)
            .map_err(|_| NativeAudioError::Closed)
    }
}
impl Drop for NativeAudioPlayer {
    fn drop(&mut self) {
        let _ = self.send(Command {
            source: None,
            replace: false,
            release: true,
            play: false,
            volume: 1.,
            report_errors: false,
            label: String::new(),
        });
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeAudioError {
    Unavailable,
    Closed,
    InvalidVolume,
}
impl std::fmt::Display for NativeAudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "native audio unavailable",
            Self::Closed => "audio command queue closed",
            Self::InvalidVolume => "audio volume must be finite and between 0 and 1",
        })
    }
}
impl std::error::Error for NativeAudioError {}
