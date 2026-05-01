//! Episode metadata retrieval and persistence workflow for TV shows.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use indicatif::MultiProgress;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{info, instrument, warn};

use crate::api_structs::{
    MetadataDateValue, MetadataEpisode, MetadataEpisodesRoot, MetadataImage, Tv3Error,
};
use crate::downloader;
use crate::error::{Error, Result};
use crate::http_client::{HttpClient, HttpClientTrait};
use crate::models::MediaItem;

const TV3_EPISODE_LIST_URL: &str = "https://www.3cat.cat/api/3cat/dades/?queryKey=%5B%22tira%22%2C%7B%22url%22%3A%22%2F%2Fapi.3cat.cat%2Fvideos%3F_format%3Djson%26no_agrupacio%3DPUAGR_LLSIGN%26tipus_contingut%3DPPD%26items_pagina%3D1500%26pagina%3D1%26sdom%3Dimg%26version%3D2.0%26cache%3D180%26https%3Dtrue%26master%3Dyes%26programatv_id%3D{tv_show_id}%26origen%3Dauto%26perfil%3Dpc%22%7D%5D";
const METADATA_FILE_SUFFIX: &str = "metadata";
const DEFAULT_COVER_EXTENSION: &str = "jpg";

/// Fetches episode metadata for a TV show and writes the metadata manifest and covers.
#[allow(clippy::let_and_return)] // Binding needed to satisfy Rust 2024 tail-expression drop order rules
#[instrument(skip(http_client, multi_progress), fields(tv_show_id, slug, directory))]
pub(crate) async fn write_tv_show_metadata(
    http_client: Arc<HttpClient>,
    tv_show_id: i32,
    slug: &str,
    directory: &str,
    multi_progress: &MultiProgress,
) -> anyhow::Result<()> {
    let download_client = http_client.inner().clone();

    let result = write_tv_show_metadata_with_clients(
        http_client,
        &download_client,
        tv_show_id,
        slug,
        directory,
        multi_progress,
    )
    .await;

    result
}

#[instrument(
    skip(http_client, download_client, multi_progress),
    fields(tv_show_id, slug, directory)
)]
async fn write_tv_show_metadata_with_clients<T>(
    http_client: Arc<T>,
    download_client: &Client,
    tv_show_id: i32,
    slug: &str,
    directory: &str,
    multi_progress: &MultiProgress,
) -> anyhow::Result<()>
where
    T: HttpClientTrait,
{
    tokio::fs::create_dir_all(directory)
        .await
        .with_context(|| format!("failed to create metadata output directory '{directory}'"))?;

    let episodes = fetch_episode_metadata(&http_client, tv_show_id).await?;
    let mut output_entries = Vec::with_capacity(episodes.len());

    for episode in episodes {
        let metadata_episode = map_episode_metadata(&episode);
        let media_item = media_item_from_episode(&episode);
        let cover_path = if let Some(cover_url) = select_cover_url(&episode.images) {
            match download_cover(
                download_client,
                multi_progress,
                &media_item,
                directory,
                cover_url,
            )
            .await
            {
                Ok(saved_path) => Some(saved_path),
                Err(error) => {
                    warn!(
                        "Failed to download cover for episode {} (id={}): {}",
                        media_item.title, media_item.id, error
                    );
                    None
                }
            }
        } else {
            None
        };

        output_entries.push(MetadataOutputEntry {
            title: metadata_episode.title,
            publication_date: metadata_episode.publication_date,
            emission_date: metadata_episode.emission_date,
            cover_path,
        });
    }

    let metadata_file_path = metadata_file_path(directory, slug);
    let serialized = serde_json::to_vec_pretty(&output_entries)
        .context("failed to serialize metadata manifest to JSON")?;

    tokio::fs::write(&metadata_file_path, serialized)
        .await
        .with_context(|| {
            format!(
                "failed to write metadata manifest '{}'",
                metadata_file_path.display()
            )
        })?;

    info!(
        "Wrote metadata for {} episodes to {}",
        output_entries.len(),
        metadata_file_path.display()
    );

    Ok(())
}

#[instrument(skip(http_client), fields(tv_show_id))]
async fn fetch_episode_metadata<T>(
    http_client: &Arc<T>,
    tv_show_id: i32,
) -> Result<Vec<MetadataEpisode>>
where
    T: HttpClientTrait,
{
    let url = TV3_EPISODE_LIST_URL.replace("{tv_show_id}", &tv_show_id.to_string());

    let response = http_client
        .get::<MetadataEpisodesRoot, Tv3Error>(&url, None)
        .await
        .map_err(|error| Error::Decoding(error.to_string()))?;

    Ok(response.response.items.item)
}

#[derive(Debug, PartialEq, Eq)]
struct ExtractedEpisodeMetadata {
    title: Option<String>,
    publication_date: Option<String>,
    emission_date: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct MetadataOutputEntry {
    title: Option<String>,
    publication_date: Option<String>,
    emission_date: Option<String>,
    cover_path: Option<String>,
}

fn map_episode_metadata(episode: &MetadataEpisode) -> ExtractedEpisodeMetadata {
    ExtractedEpisodeMetadata {
        title: normalize_optional_string(episode.title.clone()),
        publication_date: episode
            .publication_date
            .clone()
            .and_then(MetadataDateValue::into_output_string)
            .and_then(normalize_string),
        emission_date: episode
            .emission_date
            .clone()
            .and_then(MetadataDateValue::into_output_string)
            .and_then(normalize_string),
    }
}

fn media_item_from_episode(episode: &MetadataEpisode) -> MediaItem {
    let title = normalize_optional_string(episode.title.clone())
        .unwrap_or_else(|| episode.permatitle.clone());

    MediaItem {
        id: episode.id,
        title,
        video_url: None,
        subtitle_url: None,
        episode_number: Some(episode.number_of_episode),
        tv_show_name: Some(episode.tv_show_name.clone()),
    }
}

fn select_cover_url(images: &[MetadataImage]) -> Option<&str> {
    images.iter().find_map(|image| {
        let size_matches = image
            .size
            .as_deref()
            .is_some_and(|size| size.eq_ignore_ascii_case("master"));
        let relation_matches = image
            .relation_name
            .as_deref()
            .is_some_and(|relation| relation == "KEYVIDEO");

        if size_matches && relation_matches {
            Some(image.url.as_str())
        } else {
            None
        }
    })
}

async fn download_cover(
    client: &Client,
    multi_progress: &MultiProgress,
    item: &MediaItem,
    directory: &str,
    cover_url: &str,
) -> Result<String> {
    let extension = extension_from_url(cover_url);
    let file_name = format!("{}-cover-{}.{}", item.filename_stem()?, item.id, extension);
    let output_path = Path::new(directory).join(&file_name);
    let output_path_str = output_path
        .to_str()
        .ok_or_else(|| Error::InvalidPathEncoding(output_path.display().to_string()))?;

    downloader::download_content(
        cover_url,
        output_path_str,
        &file_name,
        multi_progress,
        client,
    )
    .await?;

    Ok(file_name)
}

fn extension_from_url(url: &str) -> String {
    let without_query = url.split('?').next().unwrap_or(url);
    let path = Path::new(without_query);

    path.extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| !extension.is_empty())
        .map(|extension| extension.to_ascii_lowercase())
        .unwrap_or_else(|| DEFAULT_COVER_EXTENSION.to_string())
}

fn metadata_file_path(directory: &str, slug: &str) -> PathBuf {
    Path::new(directory).join(format!("{slug}-{METADATA_FILE_SUFFIX}.json"))
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(normalize_string)
}

fn normalize_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use indicatif::MultiProgress;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::http_client::mock::MockHttpClient;

    #[test]
    fn test_should_extract_requested_metadata_and_cover_variant() {
        let episode = MetadataEpisode {
            id: 42,
            number_of_episode: 7,
            permatitle: "fallback-title".to_string(),
            title: Some("Episode title".to_string()),
            tv_show_name: "Sample show".to_string(),
            publication_date: Some(MetadataDateValue::Text("2024-01-01".to_string())),
            emission_date: Some(MetadataDateValue::Structured(
                crate::api_structs::MetadataDateFields {
                    text: Some("01/02/2024".to_string()),
                    utc: Some("2024-02-01T00:00:00Z".to_string()),
                },
            )),
            images: vec![
                MetadataImage {
                    size: Some("small".to_string()),
                    relation_name: Some("KEYVIDEO".to_string()),
                    url: "https://example.invalid/small.jpg".to_string(),
                },
                MetadataImage {
                    size: Some("master".to_string()),
                    relation_name: Some("KEYVIDEO".to_string()),
                    url: "https://example.invalid/master.jpg".to_string(),
                },
            ],
        };

        assert_eq!(
            map_episode_metadata(&episode),
            ExtractedEpisodeMetadata {
                title: Some("Episode title".to_string()),
                publication_date: Some("2024-01-01".to_string()),
                emission_date: Some("2024-02-01T00:00:00Z".to_string()),
            }
        );
        assert_eq!(
            select_cover_url(&episode.images),
            Some("https://example.invalid/master.jpg")
        );
    }

    #[test]
    fn test_should_match_cover_variant_when_live_payload_uses_uppercase_master() {
        let images = vec![MetadataImage {
            size: Some("MASTER".to_string()),
            relation_name: Some("KEYVIDEO".to_string()),
            url: "https://example.invalid/uppercase-size.jpg".to_string(),
        }];

        assert_eq!(
            select_cover_url(&images),
            Some("https://example.invalid/uppercase-size.jpg")
        );
    }

    #[test]
    fn test_should_not_match_cover_variant_when_relation_name_casing_differs() {
        let images = vec![MetadataImage {
            size: Some("master".to_string()),
            relation_name: Some("keyvideo".to_string()),
            url: "https://example.invalid/lowercase-relation.jpg".to_string(),
        }];

        assert_eq!(select_cover_url(&images), None);
    }

    #[test]
    fn test_should_deserialize_metadata_images_when_api_uses_text_field_for_url() {
        let response_json = r#"{
            "resposta": {
                "items": {
                    "item": [
                        {
                            "id": 101,
                            "capitol": 1,
                            "permatitle": "episode-one",
                            "titol": "Episode One",
                            "programa": "Sample Show",
                            "data_publicacio": "2024-01-01",
                            "data_emissio": null,
                            "imatges": [
                                {
                                    "mida": "1014x570",
                                    "rel_name": "KEYVIDEO",
                                    "text": "https://example.invalid/cover.jpg"
                                }
                            ]
                        }
                    ]
                }
            }
        }"#;

        let response: MetadataEpisodesRoot =
            serde_json::from_str(response_json).expect("metadata response should parse");

        assert_eq!(response.response.items.item.len(), 1);
        assert_eq!(
            response.response.items.item[0].images,
            vec![MetadataImage {
                size: Some("1014x570".to_string()),
                relation_name: Some("KEYVIDEO".to_string()),
                url: "https://example.invalid/cover.jpg".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn test_should_persist_metadata_json_and_download_available_covers() {
        let temp_dir = unique_test_directory("metadata");
        std::fs::create_dir_all(&temp_dir).expect("test directory should be created");

        let image_bytes = b"fake-image".to_vec();
        let (server_handle, cover_url) = start_single_response_server(image_bytes.clone()).await;

        let response_json = format!(
            r#"{{
                "resposta": {{
                    "items": {{
                        "item": [
                            {{
                                "id": 101,
                                "capitol": 1,
                                "permatitle": "episode-one",
                                "titol": "Episode One",
                                "programa": "Sample Show",
                                "data_publicacio": {{ "utc": "2024-01-01T00:00:00Z" }},
                                "data_emissio": "2024-01-02",
                                "imatges": [
                                    {{ "mida": "small", "rel_name": "KEYVIDEO", "url": "{cover_url}" }},
                                    {{ "mida": "master", "rel_name": "KEYVIDEO", "url": "{cover_url}" }}
                                ]
                            }},
                            {{
                                "id": 102,
                                "capitol": 2,
                                "permatitle": "episode-two",
                                "titol": "",
                                "programa": "Sample Show",
                                "data_publicacio": null,
                                "data_emissio": {{ "text": "03/01/2024" }},
                                "imatges": [
                                    {{ "mida": "master", "rel_name": "OTHER", "url": "{cover_url}" }}
                                ]
                            }}
                        ]
                    }}
                }}
            }}"#
        );

        let http_client = Arc::new(MockHttpClient::new(vec![response_json.as_str()]));
        let reqwest_client = Client::new();

        write_tv_show_metadata_with_clients(
            http_client,
            &reqwest_client,
            777,
            "sample-show",
            temp_dir
                .to_str()
                .expect("temp dir path should be valid utf-8"),
            &MultiProgress::new(),
        )
        .await
        .expect("metadata workflow should succeed");

        server_handle.await.expect("server task should complete");

        let metadata_path = temp_dir.join("sample-show-metadata.json");
        let metadata_json = tokio::fs::read_to_string(&metadata_path)
            .await
            .expect("metadata file should be readable");
        let entries: Vec<MetadataOutputEntry> =
            serde_json::from_str(&metadata_json).expect("metadata JSON should parse");

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title.as_deref(), Some("Episode One"));
        assert_eq!(
            entries[0].publication_date.as_deref(),
            Some("2024-01-01T00:00:00Z")
        );
        assert_eq!(entries[0].emission_date.as_deref(), Some("2024-01-02"));
        assert_eq!(
            entries[0].cover_path.as_deref(),
            Some("1-episode-one-cover-101.jpg")
        );
        assert_eq!(entries[1].title, None);
        assert_eq!(entries[1].publication_date, None);
        assert_eq!(entries[1].emission_date.as_deref(), Some("03/01/2024"));
        assert_eq!(entries[1].cover_path, None);

        let saved_cover_path = temp_dir.join("1-episode-one-cover-101.jpg");
        let saved_cover = tokio::fs::read(saved_cover_path)
            .await
            .expect("cover file should exist");
        assert_eq!(saved_cover, image_bytes);

        std::fs::remove_dir_all(&temp_dir).expect("test directory should be removed");
    }

    #[tokio::test]
    async fn test_should_write_metadata_when_cover_download_returns_http_error() {
        let temp_dir = unique_test_directory("metadata-cover-http-error");
        std::fs::create_dir_all(&temp_dir).expect("test directory should be created");

        let (server_handle, cover_url) =
            start_single_response_server_with_status(404, b"not-found".to_vec()).await;

        let response_json = format!(
            r#"{{
                "resposta": {{
                    "items": {{
                        "item": [
                            {{
                                "id": 101,
                                "capitol": 1,
                                "permatitle": "episode-one",
                                "titol": "Episode One",
                                "programa": "Sample Show",
                                "data_publicacio": {{ "utc": "2024-01-01T00:00:00Z" }},
                                "data_emissio": "2024-01-02",
                                "imatges": [
                                    {{ "mida": "master", "rel_name": "KEYVIDEO", "url": "{cover_url}" }}
                                ]
                            }}
                        ]
                    }}
                }}
            }}"#
        );

        let http_client = Arc::new(MockHttpClient::new(vec![response_json.as_str()]));
        let reqwest_client = Client::new();

        write_tv_show_metadata_with_clients(
            http_client,
            &reqwest_client,
            777,
            "sample-show",
            temp_dir
                .to_str()
                .expect("temp dir path should be valid utf-8"),
            &MultiProgress::new(),
        )
        .await
        .expect("metadata workflow should succeed even if cover download fails");

        server_handle.await.expect("server task should complete");

        let metadata_path = temp_dir.join("sample-show-metadata.json");
        let metadata_json = tokio::fs::read_to_string(&metadata_path)
            .await
            .expect("metadata file should be readable");
        let entries: Vec<MetadataOutputEntry> =
            serde_json::from_str(&metadata_json).expect("metadata JSON should parse");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title.as_deref(), Some("Episode One"));
        assert_eq!(entries[0].cover_path, None);
        assert!(!temp_dir.join("1-episode-one-cover-101.jpg").exists());

        std::fs::remove_dir_all(&temp_dir).expect("test directory should be removed");
    }

    async fn start_single_response_server(body: Vec<u8>) -> (tokio::task::JoinHandle<()>, String) {
        start_single_response_server_with_status(200, body).await
    }

    async fn start_single_response_server_with_status(
        status_code: u16,
        body: Vec<u8>,
    ) -> (tokio::task::JoinHandle<()>, String) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have local addr");
        let url = format!("http://{address}/cover.jpg");

        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("server should accept");
            let mut buffer = [0_u8; 1024];
            let _ = stream.read(&mut buffer).await;

            let status_text = match status_code {
                200 => "OK",
                404 => "Not Found",
                500 => "Internal Server Error",
                _ => "Test Response",
            };

            let response = format!(
                "HTTP/1.1 {status_code} {status_text}\r\nContent-Length: {}\r\nContent-Type: image/jpeg\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("response headers should be written");
            stream
                .write_all(&body)
                .await
                .expect("response body should be written");
        });

        (handle, url)
    }

    fn unique_test_directory(prefix: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("cat_show_downloader-{prefix}-{timestamp}"))
    }
}
