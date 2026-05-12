//! Library crate for downloading TV shows and movies from 3cat.cat.
//!
//! This crate provides the core logic for resolving media slugs, fetching
//! episode metadata, downloading video and subtitle files, and optionally
//! embedding subtitles via ffmpeg. The binary crate (`main.rs`) is a thin
//! entry point that sets up tracing, builds the Tokio runtime, and delegates
//! to [`run()`].

mod api_structs;
mod cli;
mod downloader;
mod error;
mod ffmpeg;
mod http_client;
mod media_resolver;
mod metadata;
mod models;
mod movie;
mod scheduler;
pub mod subtitle_cleaner;
mod tv_show;
mod yt_dlp;

use std::io;
use std::sync::Arc;

use indicatif::MultiProgress;
use tracing::{info, instrument, warn};
use tracing_subscriber::fmt::MakeWriter;

pub use crate::cli::CatShowDownloaderArgs;
use crate::cli::{Command, DownloadArgs, MetadataArgs};
use crate::media_resolver::MediaType;
use crate::models::{DownloadParams, MissingSubtitlePolicy, SubtitleMode};

/// A [`MakeWriter`] implementation that routes output through [`MultiProgress::println`].
///
/// This ensures that tracing log lines are printed above the progress bars
/// without disrupting their rendering.
#[derive(Clone, Debug)]
pub struct MultiProgressWriter {
    mp: MultiProgress,
}

impl MultiProgressWriter {
    /// Creates a new writer that routes output through the given [`MultiProgress`].
    pub fn new(mp: MultiProgress) -> Self {
        Self { mp }
    }
}

/// Per-event writer that buffers bytes and flushes complete lines via [`MultiProgress::println`].
///
/// This type is an implementation detail of [`MultiProgressWriter`] and should
/// not be constructed directly.
#[derive(Debug)]
pub struct MultiProgressLineWriter {
    mp: MultiProgress,
    buf: Vec<u8>,
}

impl<'a> MakeWriter<'a> for MultiProgressWriter {
    type Writer = MultiProgressLineWriter;

    fn make_writer(&'a self) -> Self::Writer {
        MultiProgressLineWriter {
            mp: self.mp.clone(),
            buf: Vec::new(),
        }
    }
}

impl io::Write for MultiProgressLineWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&self.buf).trim_end().to_string();
            self.mp.println(&line)?;
            self.buf.clear();
        }
        Ok(())
    }
}

impl Drop for MultiProgressLineWriter {
    fn drop(&mut self) {
        let _ = io::Write::flush(self);
    }
}

fn resolve_subtitle_settings(
    skip_subtitles: bool,
    allow_missing_subtitles: bool,
    ffmpeg_available: bool,
) -> (SubtitleMode, MissingSubtitlePolicy) {
    let subtitle_mode = if skip_subtitles {
        SubtitleMode::Skip
    } else if ffmpeg_available {
        SubtitleMode::Embed
    } else {
        SubtitleMode::Download
    };

    let missing_subtitle_policy = if allow_missing_subtitles {
        MissingSubtitlePolicy::AllowMissing
    } else {
        MissingSubtitlePolicy::Strict
    };

    (subtitle_mode, missing_subtitle_policy)
}

/// Runs the main application logic.
///
/// Resolves the provided slug as a TV show or movie, determines subtitle
/// handling mode, and dispatches the appropriate download workflow.
///
/// # Errors
///
/// Returns an error if media resolution, subtitle processing, or downloading
/// fails.
#[allow(clippy::let_and_return)] // Binding needed to satisfy Rust 2024 tail-expression drop order rules
#[instrument(skip_all)]
pub async fn run(args: CatShowDownloaderArgs, multi_progress: MultiProgress) -> anyhow::Result<()> {
    match args.command {
        Command::Download(download_args) => run_download(download_args, multi_progress).await,
        Command::Metadata(metadata_args) => run_metadata(metadata_args, multi_progress).await,
    }
}

#[allow(clippy::let_and_return)] // Binding needed to satisfy Rust 2024 tail-expression drop order rules
#[instrument(skip_all)]
async fn run_download(args: DownloadArgs, multi_progress: MultiProgress) -> anyhow::Result<()> {
    if args.start_from_episode < 1 {
        anyhow::bail!(
            "start_from_episode must be at least 1, got {}",
            args.start_from_episode
        );
    }

    if args.fix_existing_subtitles {
        subtitle_cleaner::fix_existing_subtitles(&args.target.directory)?;
    }

    let (ffmpeg_available, yt_dlp_available) =
        tokio::join!(ffmpeg::is_available(), yt_dlp::is_available());

    if args.embed_existing_subtitles {
        if !ffmpeg_available {
            anyhow::bail!(
                "--embed-existing-subtitles requires ffmpeg, but ffmpeg was not found on PATH"
            );
        }
        ffmpeg::embed_existing_subtitles(&args.target.directory).await?;
    }

    let http_client = http_client::http_client();
    let media = media_resolver::get_media_id(&args.target.slug).await?;

    let (subtitle_mode, missing_subtitle_policy) = resolve_subtitle_settings(
        args.skip_subtitles,
        args.allow_missing_subtitles,
        ffmpeg_available,
    );

    if subtitle_mode == SubtitleMode::Embed {
        info!("ffmpeg detected, subtitles will be embedded into video files");
    } else if subtitle_mode == SubtitleMode::Download {
        warn!("ffmpeg not found, subtitles will be downloaded as separate .vtt files");
    }

    if yt_dlp_available {
        info!("yt-dlp detected, using it as the download backend");
    }

    let params = DownloadParams {
        http_client,
        subtitle_mode,
        missing_subtitle_policy,
        concurrent_downloads: args.concurrent_downloads,
        multi_progress,
        directory: Arc::from(args.target.directory.as_str()),
        yt_dlp_available,
    };

    let result = match media {
        MediaType::TvShow(id) => tv_show::download(id, args.start_from_episode, &params).await,
        MediaType::Movie { id, slug } => movie::download(id, &slug, &params).await,
    };

    result
}

#[instrument(skip_all)]
async fn run_metadata(args: MetadataArgs, multi_progress: MultiProgress) -> anyhow::Result<()> {
    let MetadataArgs {
        target,
        image_format,
    } = args;
    let http_client = http_client::http_client();
    let media = media_resolver::get_media_id(&target.slug).await?;

    let tv_show_id = match media {
        MediaType::TvShow(id) => id,
        MediaType::Movie { slug, .. } => {
            anyhow::bail!(
                "metadata subcommand only supports TV show slugs, but '{slug}' resolved to a movie"
            );
        }
    };

    metadata::write_tv_show_metadata(
        http_client,
        tv_show_id,
        &target.slug,
        &target.directory,
        image_format,
        &multi_progress,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::resolve_subtitle_settings;
    use crate::models::{MissingSubtitlePolicy, SubtitleMode};

    #[test]
    fn test_should_prioritize_skip_subtitles_over_allow_missing_subtitles() {
        let (subtitle_mode, missing_subtitle_policy) = resolve_subtitle_settings(true, true, true);

        assert_eq!(subtitle_mode, SubtitleMode::Skip);
        assert_eq!(missing_subtitle_policy, MissingSubtitlePolicy::AllowMissing);
    }
}
