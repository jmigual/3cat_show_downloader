//! Command-line argument definitions for the 3cat media downloader.

use clap::{Args, Parser, Subcommand};

/// Command-line arguments for the 3cat media downloader.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct CatShowDownloaderArgs {
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Top-level commands supported by the downloader.
#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    /// Download episodes or movies and any configured subtitles.
    Download(DownloadArgs),
    /// Retrieve episode metadata and assets without downloading video files.
    Metadata(MetadataArgs),
}

/// Target selection options for commands that operate on a TV show or movie slug.
#[derive(Args, Debug)]
pub(crate) struct DownloadTargetArgs {
    /// Slug of the TV show or movie (e.g. "bola-de-drac" from https://www.3cat.cat/3cat/bola-de-drac/)
    pub(crate) slug: String,

    /// Directory to save the downloaded files
    #[arg(short, long)]
    pub(crate) directory: String,
}

/// Target selection options for commands that operate on a TV show slug.
#[derive(Args, Debug)]
pub(crate) struct MetadataTargetArgs {
    /// Slug of the TV show (e.g. "bola-de-drac" from https://www.3cat.cat/3cat/bola-de-drac/)
    pub(crate) slug: String,

    /// Directory to save the downloaded files
    #[arg(short, long)]
    pub(crate) directory: String,
}

/// Arguments for the download workflow.
#[derive(Args, Debug)]
pub(crate) struct DownloadArgs {
    #[command(flatten)]
    pub(crate) target: DownloadTargetArgs,

    /// Episode number to start from (ignored for movies)
    #[arg(short, long, default_value_t = 1)]
    pub(crate) start_from_episode: i32,

    /// Number of files to download concurrently (1-10)
    #[arg(short, long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..=10))]
    pub(crate) concurrent_downloads: u8,

    /// Skip downloading subtitles
    #[arg(long, default_value_t = false)]
    pub(crate) skip_subtitles: bool,

    /// Continue downloading when subtitles are unavailable
    #[arg(long, default_value_t = false)]
    pub(crate) allow_missing_subtitles: bool,

    /// Fix (clean) previously downloaded subtitle files in the directory
    #[arg(short, long, default_value_t = false)]
    pub(crate) fix_existing_subtitles: bool,

    /// Clean and embed existing subtitle files into their matching video files (requires ffmpeg)
    #[arg(long, default_value_t = false)]
    pub(crate) embed_existing_subtitles: bool,
}

/// Arguments for the metadata workflow.
#[derive(Args, Debug)]
pub(crate) struct MetadataArgs {
    #[command(flatten)]
    pub(crate) target: MetadataTargetArgs,
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{CatShowDownloaderArgs, Command};

    #[test]
    fn test_should_parse_download_subcommand_with_allow_missing_subtitles_flag() {
        let args = CatShowDownloaderArgs::parse_from([
            "cat_show_downloader",
            "download",
            "bola-de-drac",
            "--directory",
            "output",
            "--allow-missing-subtitles",
        ]);

        let Command::Download(download_args) = args.command else {
            panic!("expected download subcommand");
        };

        assert_eq!(download_args.target.slug, "bola-de-drac");
        assert_eq!(download_args.target.directory, "output");
        assert!(download_args.allow_missing_subtitles);
        assert!(!download_args.skip_subtitles);
    }

    #[test]
    fn test_should_default_to_strict_missing_subtitles_policy_for_download_subcommand() {
        let args = CatShowDownloaderArgs::parse_from([
            "cat_show_downloader",
            "download",
            "bola-de-drac",
            "--directory",
            "output",
        ]);

        let Command::Download(download_args) = args.command else {
            panic!("expected download subcommand");
        };

        assert!(!download_args.allow_missing_subtitles);
        assert_eq!(download_args.start_from_episode, 1);
    }

    #[test]
    fn test_should_parse_metadata_subcommand_with_required_options() {
        let args = CatShowDownloaderArgs::parse_from([
            "cat_show_downloader",
            "metadata",
            "bola-de-drac",
            "--directory",
            "output",
        ]);

        let Command::Metadata(metadata_args) = args.command else {
            panic!("expected metadata subcommand");
        };

        assert_eq!(metadata_args.target.slug, "bola-de-drac");
        assert_eq!(metadata_args.target.directory, "output");
    }

    #[test]
    fn test_should_reject_download_only_flags_for_metadata_subcommand() {
        let result = CatShowDownloaderArgs::try_parse_from([
            "cat_show_downloader",
            "metadata",
            "bola-de-drac",
            "--directory",
            "output",
            "--allow-missing-subtitles",
        ]);

        assert!(result.is_err());
    }

    #[test]
    fn test_should_describe_download_slug_as_tv_show_or_movie_in_help() {
        let mut command = CatShowDownloaderArgs::command();
        let mut help = Vec::new();

        command
            .find_subcommand_mut("download")
            .expect("download subcommand should exist")
            .write_long_help(&mut help)
            .expect("download help should render");

        let help = String::from_utf8(help).expect("help output should be utf-8");

        assert!(help.contains("Slug of the TV show or movie"));
    }

    #[test]
    fn test_should_describe_metadata_slug_as_tv_show_only_in_help() {
        let mut command = CatShowDownloaderArgs::command();
        let mut help = Vec::new();

        command
            .find_subcommand_mut("metadata")
            .expect("metadata subcommand should exist")
            .write_long_help(&mut help)
            .expect("metadata help should render");

        let help = String::from_utf8(help).expect("help output should be utf-8");

        assert!(help.contains("Slug of the TV show"));
        assert!(!help.contains("Slug of the TV show or movie"));
    }
}
