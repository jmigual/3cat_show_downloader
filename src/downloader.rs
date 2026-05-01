//! Download logic for media video and subtitle files.

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::Client;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tracing::{info, instrument, warn};

use crate::api_structs;
use crate::error::{Error, Result};
use crate::ffmpeg;
use crate::http_client::HttpClientTrait;
use crate::models::{DownloadParams, MediaItem, MissingSubtitlePolicy, SubtitleMode};
use crate::subtitle_cleaner;
use crate::yt_dlp;

const TV3_SINGLE_MEDIA_API_URL: &str =
    "https://dinamics.ccma.cat/pvideo/media.jsp?media=video&version=0s&idint={id}";

#[cfg(test)]
static TEST_SINGLE_MEDIA_API_URL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn single_media_api_url(id: i32) -> String {
    #[cfg(test)]
    let template = TEST_SINGLE_MEDIA_API_URL
        .lock()
        .expect("test single-media API URL lock poisoned")
        .clone()
        .unwrap_or_else(|| TV3_SINGLE_MEDIA_API_URL.to_string());

    #[cfg(not(test))]
    let template = TV3_SINGLE_MEDIA_API_URL.to_string();

    template.replace("{id}", &id.to_string())
}

/// Fetches metadata for a single media item and downloads its video and subtitle files.
///
/// When `params.yt_dlp_available` is `true`, delegates entirely to yt-dlp,
/// which handles format selection and subtitle extraction without a prior API
/// call. Otherwise, retrieves the video URL and subtitles from the 3cat API
/// and streams the files using the built-in HTTP downloader.
///
/// The [`SubtitleMode`] inside `params` controls whether subtitles are
/// skipped, downloaded as separate files, or embedded into the video.
///
/// # Errors
///
/// Returns an error if the metadata fetch, download, or file I/O fails.
pub async fn fetch_and_download_media(mut item: MediaItem, params: &DownloadParams) -> Result<()> {
    if params.yt_dlp_available {
        return yt_dlp::download(
            &item,
            &params.directory,
            params.subtitle_mode,
            params.missing_subtitle_policy,
            &params.multi_progress,
        )
        .await;
    }

    let api_response = params
        .http_client
        .get::<api_structs::SingleEpisodeRoot, api_structs::Tv3Error>(
            single_media_api_url(item.id).as_str(),
            None,
        )
        .await
        .map_err(|e| Error::Decoding(e.to_string()))?;

    for url in api_response.media.url {
        if !url.active {
            continue;
        }
        item.video_url = Some(url.file);
        break;
    }

    if let Some(subtitles) = api_response.subtitles.as_ref().and_then(|s| s.first()) {
        item.subtitle_url = Some(subtitles.url.clone());
    } else if params.subtitle_mode != SubtitleMode::Skip {
        handle_missing_subtitles(&item.title, params.missing_subtitle_policy)?;
    }

    let reqwest_client = params.http_client.inner();
    download_media(
        &item,
        &params.directory,
        &params.multi_progress,
        reqwest_client,
        params.subtitle_mode,
    )
    .await
}

/// Downloads the video and subtitle files for a media item to the given directory.
///
/// Skips the download if the file already exists and is non-empty.
/// Uses the provided [`MultiProgress`] to render concurrent progress bars,
/// and the shared [`Client`] for connection pooling.
///
/// # Errors
///
/// Returns an error if downloading, file I/O, or path encoding fails.
#[instrument(skip_all, fields(media_id = item.id))]
async fn download_media(
    item: &MediaItem,
    directory: &str,
    multi_progress: &MultiProgress,
    client: &Client,
    subtitle_mode: SubtitleMode,
) -> Result<()> {
    let existing_video_files = find_existing_video_files(item, directory).await?;
    let subtitle_exists = builtin_subtitle_exists(item, directory)?;

    if builtin_media_is_complete(&existing_video_files, subtitle_exists, subtitle_mode) {
        info!("Media item already exists: {}", item.filename("mp4")?);
        return Ok(());
    }

    download_data(
        item,
        directory,
        multi_progress,
        client,
        subtitle_mode,
        existing_video_files.reusable_video_path,
    )
    .await
}

#[instrument(skip_all)]
async fn download_data(
    item: &MediaItem,
    directory: &str,
    multi_progress: &MultiProgress,
    client: &Client,
    subtitle_mode: SubtitleMode,
    existing_video_path: Option<String>,
) -> Result<()> {
    let video_path = if let Some(path) = existing_video_path {
        info!("Reusing existing video at {path}");
        path
    } else {
        let Some(video_url) = &item.video_url else {
            return Err(Error::MediaDoesNotHaveVideoUrl(item.filename("mp4")?));
        };

        let video_filename = item.filename("mp4")?;
        let video_path = full_media_path(item, directory, "mp4")?;
        download_content(
            video_url,
            &video_path,
            &video_filename,
            multi_progress,
            client,
        )
        .await?;
        info!("Downloaded video to {video_path}");
        video_path
    };

    if subtitle_mode == SubtitleMode::Skip {
        return Ok(());
    }

    let subtitle_filename = item.filename("vtt")?;
    let subtitle_path = full_media_path(item, directory, "vtt")?;
    if !non_empty_file_exists(&subtitle_path) {
        let Some(subtitle_url) = &item.subtitle_url else {
            return Ok(());
        };

        download_content(
            subtitle_url,
            &subtitle_path,
            &subtitle_filename,
            multi_progress,
            client,
        )
        .await?;
    }

    subtitle_cleaner::clean_vtt_file(std::path::Path::new(&subtitle_path))?;

    if subtitle_mode == SubtitleMode::Embed {
        let track = ffmpeg::SubtitleTrack {
            path: std::path::PathBuf::from(&subtitle_path),
            lang_code: "ca".to_string(),
        };
        match ffmpeg::embed_subtitles(&video_path, &[track]).await {
            Ok(mkv_path) => {
                info!("Subtitles embedded into video {mkv_path}");
            }
            Err(e) => {
                warn!("Failed to embed subtitles into {video_path}: {e}");
                info!("Downloaded subtitle to {subtitle_path}");
            }
        }
    } else {
        info!("Downloaded subtitle to {subtitle_path}");
    }

    Ok(())
}

fn handle_missing_subtitles(
    title: &str,
    missing_subtitle_policy: MissingSubtitlePolicy,
) -> Result<()> {
    if missing_subtitle_policy.allows_missing() {
        warn!(
            "Subtitles requested for \"{title}\" but none were available; continuing without subtitles"
        );
        return Ok(());
    }

    Err(Error::NoSubtitlesAvailable(title.to_string()))
}

/// Video file extensions produced by yt-dlp or the built-in HTTP downloader.
///
/// Used by [`find_existing_video_files`] to match any video file for a given stem
/// while ignoring subtitle (`.vtt`, `.ass`) and other non-video files.
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "webm", "ts", "m4v"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ExistingVideoFiles {
    pub(crate) reusable_video_path: Option<String>,
    pub(crate) embedded_video_path: Option<String>,
}

/// Returns the local video artifacts already present for `item`.
///
/// This covers the case where yt-dlp chose an extension other than `.mp4`
/// (e.g. `.webm`) or where ffmpeg previously produced a `.mkv` after
/// embedding subtitles.  Stale `.mp4.tmp` files left by interrupted
/// HTTP downloads are cleaned up before the check.
#[instrument(skip_all)]
pub(crate) async fn find_existing_video_files(
    item: &MediaItem,
    directory: &str,
) -> Result<ExistingVideoFiles> {
    let video_path = full_media_path(item, directory, "mp4")?;
    let tmp_path = format!("{video_path}.tmp");
    let mut video_files = ExistingVideoFiles::default();

    // Clean up stale .tmp files from previous interrupted runs.
    let _ = tokio::fs::remove_file(&tmp_path).await;

    // Derive the stem (e.g. "7-episode-title") to match any video extension.
    let filename = item.filename("mp4")?;
    let stem = std::path::Path::new(&filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::InvalidPathEncoding(filename.clone()))?;

    let mut read_dir = tokio::fs::read_dir(std::path::Path::new(directory))
        .await
        .map_err(|e| Error::Downloading(e.to_string()))?;

    while let Some(entry) = read_dir
        .next_entry()
        .await
        .map_err(|e| Error::Downloading(e.to_string()))?
    {
        let entry_name = entry.file_name();
        let entry_path = std::path::Path::new(&entry_name);

        let Some(ext) = entry_path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if !VIDEO_EXTENSIONS.contains(&ext) {
            continue;
        }

        let Some(entry_stem) = entry_path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if entry_stem != stem {
            continue;
        }

        let full_path = std::path::Path::new(directory).join(&entry_name);
        let full_path_str = full_path
            .to_str()
            .ok_or_else(|| Error::InvalidPathEncoding(format!("{}", full_path.display())))?;
        if !non_empty_file_exists(full_path_str) {
            continue;
        }

        let full_path_string = full_path_str.to_string();

        if ext == "mkv" {
            if video_files.embedded_video_path.is_none() {
                video_files.embedded_video_path = Some(full_path_string);
            }
            continue;
        }

        if video_files.reusable_video_path.is_none() {
            video_files.reusable_video_path = Some(full_path_string);
        }
    }

    Ok(video_files)
}

fn builtin_subtitle_exists(item: &MediaItem, directory: &str) -> Result<bool> {
    let subtitle_path = full_media_path(item, directory, "vtt")?;
    Ok(non_empty_file_exists(&subtitle_path))
}

fn builtin_media_is_complete(
    video_files: &ExistingVideoFiles,
    subtitle_exists: bool,
    subtitle_mode: SubtitleMode,
) -> bool {
    match subtitle_mode {
        SubtitleMode::Skip => video_files.reusable_video_path.is_some(),
        SubtitleMode::Download => video_files.reusable_video_path.is_some() && subtitle_exists,
        SubtitleMode::Embed => video_files.embedded_video_path.is_some(),
    }
}

/// Returns `true` when `path` exists and has a non-zero size.
///
/// Zero-byte files left by previous failed downloads are cleaned up
/// and treated as non-existent.
fn non_empty_file_exists(path: &str) -> bool {
    let p = std::path::Path::new(path);
    if !p.exists() {
        return false;
    }
    if let Ok(metadata) = p.metadata() {
        if metadata.len() == 0 {
            let _ = std::fs::remove_file(p);
            return false;
        }
    }
    true
}

pub(crate) fn full_media_path(
    item: &MediaItem,
    directory: &str,
    extension: &str,
) -> Result<String> {
    let path = std::path::Path::new(directory).join(item.filename(extension)?);
    path.to_str()
        .map(|s| s.to_string())
        .ok_or_else(|| Error::InvalidPathEncoding(format!("{}", path.display())))
}

#[instrument(skip_all, fields(url, path))]
pub(crate) async fn download_content(
    url: &str,
    path: &str,
    label: &str,
    multi_progress: &MultiProgress,
    client: &Client,
) -> Result<()> {
    let tmp_path = format!("{path}.tmp");

    let result = download_to_file(url, &tmp_path, label, multi_progress, client).await;

    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return result;
    }

    tokio::fs::rename(&tmp_path, path)
        .await
        .map_err(|e| Error::Downloading(e.to_string()))?;

    Ok(())
}

/// Creates a styled progress bar for download tracking, registered with the [`MultiProgress`].
///
/// # Errors
///
/// Returns an error if the progress bar template is invalid.
fn create_progress_bar(
    total_size: u64,
    label: &str,
    multi_progress: &MultiProgress,
) -> Result<ProgressBar> {
    let pb = multi_progress.add(ProgressBar::new(total_size));
    pb.set_style(
        ProgressStyle::with_template(
            "{prefix:.bold} [{bar:30.cyan/blue}] {percent}% ({bytes}/{total_bytes}) {bytes_per_sec} ETA {eta}",
        )
        .map_err(|e| Error::Downloading(e.to_string()))?
        .progress_chars("█░░"),
    );
    pb.set_prefix(label.to_string());
    Ok(pb)
}

/// Creates a spinner-style progress bar when total size is unknown, registered with the [`MultiProgress`].
///
/// # Errors
///
/// Returns an error if the spinner template is invalid.
fn create_spinner(label: &str, multi_progress: &MultiProgress) -> Result<ProgressBar> {
    let pb = multi_progress.add(ProgressBar::new_spinner());
    pb.set_style(
        ProgressStyle::with_template("{prefix:.bold} {spinner:.cyan} ({bytes}) {bytes_per_sec}")
            .map_err(|e| Error::Downloading(e.to_string()))?,
    );
    pb.set_prefix(label.to_string());
    Ok(pb)
}

#[instrument(skip_all, fields(url, path))]
async fn download_to_file(
    url: &str,
    path: &str,
    label: &str,
    multi_progress: &MultiProgress,
    client: &Client,
) -> Result<()> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Downloading(e.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        return Err(Error::Downloading(format!(
            "request failed with HTTP status {status}"
        )));
    }

    let mut file = File::create(path)
        .await
        .map_err(|e| Error::Downloading(e.to_string()))?;

    let pb = match response.content_length() {
        Some(total) => create_progress_bar(total, label, multi_progress)?,
        None => create_spinner(label, multi_progress)?,
    };

    let mut downloaded: u64 = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| Error::Downloading(e.to_string()))?
    {
        file.write_all(&chunk)
            .await
            .map_err(|e| Error::Downloading(e.to_string()))?;
        downloaded += chunk.len() as u64;
        pb.set_position(downloaded);
    }

    pb.finish_and_clear();

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use indicatif::MultiProgress;
    use reqwest::Client;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;
    use tracing_subscriber::fmt::MakeWriter;

    use super::{
        TEST_SINGLE_MEDIA_API_URL, builtin_media_is_complete, download_content,
        fetch_and_download_media, find_existing_video_files, handle_missing_subtitles,
    };
    use crate::error::Error;
    use crate::http_client::{HttpClient, HttpClientTrait};
    use crate::models::{DownloadParams, MediaItem, MissingSubtitlePolicy, SubtitleMode};

    #[derive(Clone, Debug, Default)]
    struct LogCapture {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl LogCapture {
        fn contents(&self) -> String {
            String::from_utf8(
                self.buffer
                    .lock()
                    .expect("log buffer lock poisoned")
                    .clone(),
            )
            .expect("log buffer should contain valid UTF-8")
        }
    }

    #[derive(Debug)]
    struct LogWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer
                .lock()
                .expect("log buffer lock poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for LogCapture {
        type Writer = LogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            LogWriter {
                buffer: Arc::clone(&self.buffer),
            }
        }
    }

    async fn spawn_video_server(
        body: &'static [u8],
    ) -> io::Result<(String, JoinHandle<io::Result<()>>)> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await?;

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await?;
            stream.write_all(body).await?;
            stream.shutdown().await
        });

        Ok((format!("http://{address}/video.mp4"), server))
    }

    async fn spawn_json_server(body: String) -> io::Result<(String, JoinHandle<io::Result<()>>)> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await?;

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await?;
            stream.shutdown().await
        });

        Ok((
            format!("http://{address}/media?media=video&version=0s&idint={{id}}"),
            server,
        ))
    }

    async fn spawn_status_server(
        status_code: u16,
        body: &'static [u8],
    ) -> io::Result<(String, JoinHandle<io::Result<()>>)> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await?;

            let status_text = match status_code {
                200 => "OK",
                404 => "Not Found",
                500 => "Internal Server Error",
                _ => "Test Response",
            };
            let response = format!(
                "HTTP/1.1 {status_code} {status_text}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await?;
            stream.write_all(body).await?;
            stream.shutdown().await
        });

        Ok((format!("http://{address}/cover.jpg"), server))
    }

    #[derive(Debug)]
    struct TestSingleMediaApiUrlGuard;

    impl TestSingleMediaApiUrlGuard {
        fn set(url: String) -> Self {
            *TEST_SINGLE_MEDIA_API_URL
                .lock()
                .expect("test single-media API URL lock poisoned") = Some(url);
            Self
        }
    }

    impl Drop for TestSingleMediaApiUrlGuard {
        fn drop(&mut self) {
            *TEST_SINGLE_MEDIA_API_URL
                .lock()
                .expect("test single-media API URL lock poisoned") = None;
        }
    }

    fn create_test_directory(test_name: &str) -> PathBuf {
        let unique_suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "cat_show_downloader_{test_name}_{}_{}",
            std::process::id(),
            unique_suffix
        ))
    }

    #[test]
    fn test_should_error_when_missing_subtitles_are_strict() {
        let result = handle_missing_subtitles("Episode title", MissingSubtitlePolicy::Strict);

        assert!(
            matches!(result, Err(Error::NoSubtitlesAvailable(title)) if title == "Episode title")
        );
    }

    #[test]
    fn test_should_allow_missing_subtitles_when_policy_is_permissive() {
        let result = handle_missing_subtitles("Episode title", MissingSubtitlePolicy::AllowMissing);

        assert!(result.is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_should_emit_single_warning_when_missing_subtitles_are_allowed_for_builtin_downloader()
     {
        let log_capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(log_capture.clone())
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let directory = create_test_directory("missing_subtitles_warning");
        std::fs::create_dir_all(&directory).expect("test directory should be created");

        let (video_url, server) = spawn_video_server(b"video-bytes")
            .await
            .expect("test video server should start");
        let api_response = format!(
            r#"{{"media":{{"url":[{{"file":"{video_url}","active":true}}]}},"subtitols":null}}"#
        );
        let (api_url, api_server) = spawn_json_server(api_response)
            .await
            .expect("test API server should start");
        let _api_url_guard = TestSingleMediaApiUrlGuard::set(api_url);

        let item = MediaItem {
            id: 42,
            title: "Episode title".to_string(),
            video_url: None,
            subtitle_url: None,
            episode_number: Some(1),
            tv_show_name: Some("Show".to_string()),
        };
        let result = fetch_and_download_media(
            item,
            &DownloadParams {
                http_client: Arc::new(HttpClient::new()),
                subtitle_mode: SubtitleMode::Download,
                missing_subtitle_policy: MissingSubtitlePolicy::AllowMissing,
                concurrent_downloads: 1,
                multi_progress: MultiProgress::new(),
                directory: Arc::from(
                    directory
                        .to_str()
                        .expect("test directory path should be valid UTF-8"),
                ),
                yt_dlp_available: false,
            },
        )
        .await;

        let server_result = server.await.expect("video server task should join");
        let api_server_result = api_server.await.expect("API server task should join");
        std::fs::remove_dir_all(&directory).expect("test directory should be removed");

        assert!(result.is_ok());
        assert!(server_result.is_ok());
        assert!(api_server_result.is_ok());

        let warning = "Subtitles requested for \"Episode title\" but none were available; continuing without subtitles";
        let warning_count = log_capture.contents().matches(warning).count();

        assert_eq!(
            warning_count, 1,
            "expected exactly one missing-subtitles warning"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_should_download_missing_subtitle_without_redownloading_existing_video() {
        let directory = create_test_directory("subtitle_recovery_builtin");
        std::fs::create_dir_all(&directory).expect("test directory should be created");

        let item = MediaItem {
            id: 77,
            title: "Episode title".to_string(),
            video_url: None,
            subtitle_url: None,
            episode_number: Some(1),
            tv_show_name: Some("Show".to_string()),
        };
        let video_path = directory.join(item.filename("mp4").expect("video filename should build"));
        let subtitle_path = directory.join(
            item.filename("vtt")
                .expect("subtitle filename should build"),
        );
        std::fs::write(&video_path, b"existing-video-bytes")
            .expect("existing video file should be written");

        let (subtitle_url, subtitle_server) =
            spawn_video_server(b"WEBVTT\n\n00:00.000 --> 00:01.000\nRecovered subtitle\n")
                .await
                .expect("subtitle server should start");
        let api_response = format!(
            r#"{{"media":{{"url":[{{"file":"http://127.0.0.1:9/video.mp4","active":true}}]}},"subtitols":[{{"url":"{subtitle_url}"}}]}}"#
        );
        let (api_url, api_server) = spawn_json_server(api_response)
            .await
            .expect("test API server should start");
        let _api_url_guard = TestSingleMediaApiUrlGuard::set(api_url);

        let result = fetch_and_download_media(
            item,
            &DownloadParams {
                http_client: Arc::new(HttpClient::new()),
                subtitle_mode: SubtitleMode::Download,
                missing_subtitle_policy: MissingSubtitlePolicy::Strict,
                concurrent_downloads: 1,
                multi_progress: MultiProgress::new(),
                directory: Arc::from(
                    directory
                        .to_str()
                        .expect("test directory path should be valid UTF-8"),
                ),
                yt_dlp_available: false,
            },
        )
        .await;

        let subtitle_server_result = subtitle_server
            .await
            .expect("subtitle server task should join");
        let api_server_result = api_server.await.expect("API server task should join");

        let video_bytes = std::fs::read(&video_path).expect("existing video should still exist");
        let subtitle_contents =
            std::fs::read_to_string(&subtitle_path).expect("subtitle file should be present");

        std::fs::remove_dir_all(&directory).expect("test directory should be removed");

        assert!(result.is_ok());
        assert!(subtitle_server_result.is_ok());
        assert!(api_server_result.is_ok());
        assert_eq!(video_bytes, b"existing-video-bytes");
        assert!(subtitle_contents.contains("Recovered subtitle"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_should_treat_existing_video_without_subtitle_as_incomplete_for_download_mode() {
        let directory = create_test_directory("download_incomplete_without_subtitle");
        std::fs::create_dir_all(&directory).expect("test directory should be created");

        let item = MediaItem {
            id: 11,
            title: "Episode title".to_string(),
            video_url: None,
            subtitle_url: None,
            episode_number: Some(1),
            tv_show_name: Some("Show".to_string()),
        };
        let video_path = directory.join(item.filename("mp4").expect("video filename should build"));
        std::fs::write(&video_path, b"existing-video-bytes")
            .expect("existing video file should be written");

        let video_files = find_existing_video_files(
            &item,
            directory
                .to_str()
                .expect("test directory path should be valid UTF-8"),
        )
        .await
        .expect("existing video files should be resolved");

        std::fs::remove_dir_all(&directory).expect("test directory should be removed");

        assert!(!builtin_media_is_complete(
            &video_files,
            false,
            SubtitleMode::Download,
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_should_treat_existing_mkv_as_complete_for_embed_mode() {
        let directory = create_test_directory("embed_complete_with_mkv");
        std::fs::create_dir_all(&directory).expect("test directory should be created");

        let item = MediaItem {
            id: 12,
            title: "Episode title".to_string(),
            video_url: None,
            subtitle_url: None,
            episode_number: Some(1),
            tv_show_name: Some("Show".to_string()),
        };
        let video_path = directory.join(item.filename("mkv").expect("video filename should build"));
        std::fs::write(&video_path, b"embedded-video-bytes")
            .expect("embedded video file should be written");

        let video_files = find_existing_video_files(
            &item,
            directory
                .to_str()
                .expect("test directory path should be valid UTF-8"),
        )
        .await
        .expect("existing video files should be resolved");

        std::fs::remove_dir_all(&directory).expect("test directory should be removed");

        assert!(builtin_media_is_complete(
            &video_files,
            false,
            SubtitleMode::Embed,
        ));
    }

    #[tokio::test]
    async fn test_should_not_leave_final_file_when_download_returns_http_error() {
        let directory = create_test_directory("download_http_error");
        std::fs::create_dir_all(&directory).expect("test directory should be created");

        let (url, server) = spawn_status_server(404, b"not-found")
            .await
            .expect("test status server should start");
        let destination = directory.join("cover.jpg");
        let destination_str = destination
            .to_str()
            .expect("test directory path should be valid UTF-8");

        let result = download_content(
            &url,
            destination_str,
            "cover.jpg",
            &MultiProgress::new(),
            &Client::new(),
        )
        .await;

        let server_result = server.await.expect("status server task should join");

        assert!(matches!(
            result,
            Err(Error::Downloading(message)) if message.contains("HTTP status 404")
        ));
        assert!(server_result.is_ok());
        assert!(!destination.exists());
        assert!(!directory.join("cover.jpg.tmp").exists());

        std::fs::remove_dir_all(&directory).expect("test directory should be removed");
    }
}
