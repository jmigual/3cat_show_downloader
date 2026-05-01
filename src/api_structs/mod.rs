//! Serde deserialization structs for the shared 3cat media API responses.

use std::fmt::Display;

use serde::{Deserialize, Serialize};

/// Placeholder error type returned by the 3cat API on failure.
#[derive(Debug, Deserialize)]
pub struct Tv3Error {}

impl Display for Tv3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Error")
    }
}

/// Root wrapper for the single-media detail response.
#[derive(Debug, Deserialize)]
pub struct SingleEpisodeRoot {
    /// Video file metadata.
    pub media: SingleEpisodeMedia,
    /// Available subtitle tracks (absent or empty when the episode has none).
    #[serde(rename = "subtitols")]
    pub subtitles: Option<Vec<SingleEpisodeSubtitles>>,
}

/// Container for the list of video URLs.
#[derive(Debug, Deserialize)]
pub struct SingleEpisodeMedia {
    /// Available video file URLs with their active status.
    pub url: Vec<UrlMetadata>,
}

/// A single video URL entry from the media API.
#[derive(Debug, Deserialize)]
pub struct UrlMetadata {
    /// Direct URL to the video file.
    pub file: String,
    /// Whether this URL is currently active/available.
    pub active: bool,
}

/// A single subtitle track entry from the media API.
#[derive(Debug, Deserialize)]
pub struct SingleEpisodeSubtitles {
    /// Direct URL to the subtitle file.
    pub url: String,
}

/// Root wrapper for the TV show episode list response used by the metadata workflow.
#[derive(Debug, Deserialize)]
pub struct MetadataEpisodesRoot {
    /// The main response payload.
    #[serde(rename = "resposta")]
    pub response: MetadataMainResponse,
}

/// Outer response containing the episode collection.
#[derive(Debug, Deserialize)]
pub struct MetadataMainResponse {
    /// Collection of episode items.
    pub items: MetadataItems,
}

/// Wrapper around the metadata episode item list.
#[derive(Debug, Deserialize)]
pub struct MetadataItems {
    /// Individual episode entries.
    pub item: Vec<MetadataEpisode>,
}

/// Episode entry with the fields required by the metadata workflow.
#[derive(Debug, Deserialize)]
pub struct MetadataEpisode {
    /// Internal 3cat episode ID.
    pub id: i32,
    /// Sequential episode number within the show.
    #[serde(rename = "capitol")]
    pub number_of_episode: i32,
    /// Permanent URL-friendly title.
    #[serde(rename = "permatitle")]
    pub permatitle: String,
    /// Human-readable title.
    #[serde(rename = "titol")]
    pub title: Option<String>,
    /// Name of the TV show this episode belongs to.
    #[serde(rename = "programa")]
    pub tv_show_name: String,
    /// Publication date payload.
    #[serde(rename = "data_publicacio")]
    pub publication_date: Option<MetadataDateValue>,
    /// Emission date payload.
    #[serde(rename = "data_emissio")]
    pub emission_date: Option<MetadataDateValue>,
    /// Episode image variants.
    #[serde(default, rename = "imatges")]
    pub images: Vec<MetadataImage>,
}

/// Date value returned by 3cat APIs.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum MetadataDateValue {
    /// Date payload with separate text and UTC fields.
    Structured(MetadataDateFields),
    /// Date payload already represented as a string.
    Text(String),
}

impl MetadataDateValue {
    /// Returns the most stable string representation available for output.
    #[must_use]
    pub fn into_output_string(self) -> Option<String> {
        match self {
            Self::Structured(fields) => fields.utc.or(fields.text),
            Self::Text(value) if value.trim().is_empty() => None,
            Self::Text(value) => Some(value),
        }
    }
}

/// Structured date fields returned by some 3cat payloads.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MetadataDateFields {
    /// Human-readable date string.
    pub text: Option<String>,
    /// ISO-like UTC timestamp string.
    pub utc: Option<String>,
}

/// Image variant entry attached to an episode.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct MetadataImage {
    /// Image size name.
    #[serde(rename = "mida", alias = "label")]
    pub size: Option<String>,
    /// Image relation name.
    #[serde(rename = "rel_name", alias = "realname")]
    pub relation_name: Option<String>,
    /// Direct URL to the image.
    #[serde(alias = "file")]
    pub url: String,
}
