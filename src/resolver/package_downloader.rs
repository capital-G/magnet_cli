use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context};
use futures_util::{stream, StreamExt, TryStreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use pubgrub::SemanticVersion;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, instrument, warn};
use url::Url;

use crate::models::package::PackageKind;
use crate::models::project_manifest::MagnetManifestV1;

/// A single artifact of a package version, as listed in the index
#[derive(Debug, Clone, PartialEq)]
pub struct Artifact {
    pub name: String,
    pub version: SemanticVersion,
    pub kind: PackageKind,
    pub abi: String,
    pub platform: String,
    pub sha256: String,
}

/// An artifact which got deleted from the index
#[derive(Debug)]
struct Deleted;

impl std::fmt::Display for Deleted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "The artifact was deleted")
    }
}

impl std::error::Error for Deleted {}

/// Downloads artifacts into a cache directory and verifies their checksums.
/// Files which are already in the cache are not downloaded again.
pub struct PackageDownloader {
    client: reqwest::Client,
    cache_dir: PathBuf,
    /// the `dl` template of the index config
    url_template: String,
}

impl PackageDownloader {
    pub fn new(cache_dir: PathBuf, url_template: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            cache_dir,
            url_template,
        }
    }

    /// Fills the placeholders of the `dl` template
    pub fn artifact_url(&self, artifact: &Artifact) -> anyhow::Result<Url> {
        let url = self
            .url_template
            .replace("{name}", &MagnetManifestV1::normalize_name(&artifact.name))
            .replace("{version}", &artifact.version.to_string())
            .replace("{kind}", &artifact.kind.to_string())
            .replace("{abi}", &artifact.abi)
            .replace("{platform}", &artifact.platform);
        ensure!(
            !url.contains('{'),
            "The download URL template {} contains an unknown placeholder",
            self.url_template
        );
        url.parse()
            .with_context(|| format!("Invalid download URL {url}"))
    }

    /// `<cache>/<name>-<version>-<sha256>.tar.gz`
    pub fn cache_path(&self, artifact: &Artifact) -> anyhow::Result<PathBuf> {
        // the checksum becomes part of a path, so it must not contain e.g. `/` or `..`
        ensure!(
            artifact.sha256.len() == 64 && artifact.sha256.chars().all(|c| c.is_ascii_hexdigit()),
            "Invalid sha256 checksum {} of {} {}",
            artifact.sha256,
            artifact.name,
            artifact.version
        );
        Ok(self.cache_dir.join(format!(
            "{}-{}-{}.tar.gz",
            MagnetManifestV1::normalize_name(&artifact.name),
            artifact.version,
            artifact.sha256.to_lowercase()
        )))
    }

    /// Returns the path of the verified artifact in the cache, downloading it if necessary
    #[instrument(skip(self, progress_bar), fields(name = artifact.name, version = %artifact.version))]
    pub async fn download(
        &self,
        artifact: &Artifact,
        progress_bar: &ProgressBar,
    ) -> anyhow::Result<PathBuf> {
        let cache_path = self.cache_path(artifact)?;
        progress_bar.set_message(format!("{} {}", artifact.name, artifact.version));

        if cache_path.is_file() {
            if sha256_of_file(&cache_path).await? == artifact.sha256.to_lowercase() {
                debug!(?cache_path, "Use cached artifact");
                progress_bar.finish_with_message(format!(
                    "{} {} (cached)",
                    artifact.name, artifact.version
                ));
                return Ok(cache_path);
            }
            warn!(
                ?cache_path,
                "Cached artifact is corrupted, download it again"
            );
            tokio::fs::remove_file(&cache_path).await?;
        }

        tokio::fs::create_dir_all(&self.cache_dir)
            .await
            .with_context(|| format!("Could not create cache directory {:?}", self.cache_dir))?;
        // download next to the final location, so a partial download never ends up in the cache
        let temp_file = tempfile::NamedTempFile::new_in(&self.cache_dir)?;
        let url = self.artifact_url(artifact)?;
        let sha256 = match self.fetch(&url, temp_file.path(), progress_bar).await {
            Ok(sha256) => sha256,
            Err(err) if err.is::<Deleted>() => bail!(
                "{} {} was deleted from the index ({url} answers 410 Gone). Run `magnet update {}` to resolve it again",
                artifact.name,
                artifact.version,
                artifact.name
            ),
            Err(err) => return Err(err.context(format!("Could not download {url}"))),
        };
        ensure!(
            sha256 == artifact.sha256.to_lowercase(),
            "Checksum mismatch for {} {} from {url}: expected {}, got {sha256}",
            artifact.name,
            artifact.version,
            artifact.sha256
        );
        temp_file
            .persist(&cache_path)
            .with_context(|| format!("Could not move download to {cache_path:?}"))?;

        progress_bar.finish_with_message(format!("{} {}", artifact.name, artifact.version));
        Ok(cache_path)
    }

    /// Streams `url` into `target` and returns the sha256 of its content.
    /// `file://` URLs are supported, e.g. for a local mirror or tests.
    async fn fetch(
        &self,
        url: &Url,
        target: &Path,
        progress_bar: &ProgressBar,
    ) -> anyhow::Result<String> {
        let mut file = tokio::fs::File::create(target).await?;
        let mut hasher = Sha256::new();

        if url.scheme() == "file" {
            let path = url
                .to_file_path()
                .map_err(|_| anyhow::anyhow!("Invalid file URL {url}"))?;
            let mut source = tokio::fs::File::open(&path).await?;
            progress_bar.set_length(source.metadata().await?.len());
            let mut buffer = vec![0; 64 * 1024];
            loop {
                let read = source.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
                file.write_all(&buffer[..read]).await?;
                progress_bar.inc(read as u64);
            }
        } else {
            let response = self.client.get(url.clone()).send().await?;
            // the server answers deleted artifacts with 410 (DESIGN.md "Deletion")
            if response.status() == reqwest::StatusCode::GONE {
                return Err(Deleted.into());
            }
            let response = response.error_for_status()?;
            if let Some(length) = response.content_length() {
                progress_bar.set_length(length);
            }
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                hasher.update(&chunk);
                file.write_all(&chunk).await?;
                progress_bar.inc(chunk.len() as u64);
            }
        }

        file.flush().await?;
        Ok(format!("{:x}", hasher.finalize()))
    }

    /// Downloads all artifacts, at most `parallel` at a time.
    /// Returns the cache paths in the order of `artifacts`.
    pub async fn download_all(
        &self,
        artifacts: &[Artifact],
        parallel: usize,
    ) -> anyhow::Result<Vec<PathBuf>> {
        let progress = MultiProgress::new();
        let style = ProgressStyle::with_template("{bar:30} {bytes:>10}/{total_bytes:10} {msg}")?;

        let results: Vec<anyhow::Result<PathBuf>> = stream::iter(artifacts)
            .map(|artifact| {
                let progress_bar = progress.add(ProgressBar::new(0).with_style(style.clone()));
                async move { self.download(artifact, &progress_bar).await }
            })
            .buffered(parallel.max(1))
            .collect()
            .await;

        // report every failed download, not only the first one
        let errors: Vec<String> = results
            .iter()
            .filter_map(|result| result.as_ref().err().map(|err| format!("{err:#}")))
            .collect();
        if !errors.is_empty() {
            bail!("Could not download all packages:\n{}", errors.join("\n"));
        }
        stream::iter(results).try_collect().await
    }
}

async fn sha256_of_file(path: &Path) -> anyhow::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    const OFFICIAL_TEMPLATE: &str = "https://magnet.supercollider.online/dl/{name}/{version}/{name}-{version}-{kind}-{abi}-{platform}.tar.gz";

    fn sha256(content: &[u8]) -> String {
        format!("{:x}", Sha256::digest(content))
    }

    fn artifact(name: &str, content: &[u8]) -> Artifact {
        Artifact {
            name: name.to_string(),
            version: SemanticVersion::new(1, 2, 3),
            kind: PackageKind::Quark,
            abi: "any".to_string(),
            platform: "any".to_string(),
            sha256: sha256(content),
        }
    }

    /// a directory with `<name>.tar.gz` files and a `file://` template pointing to them
    fn local_mirror(files: &[(&str, &[u8])]) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            std::fs::write(dir.path().join(format!("{name}.tar.gz")), content).unwrap();
        }
        let template = format!(
            "{}{{name}}.tar.gz",
            Url::from_directory_path(dir.path()).unwrap()
        );
        (dir, template)
    }

    #[test]
    fn test_artifact_url() {
        let downloader = PackageDownloader::new(PathBuf::new(), OFFICIAL_TEMPLATE.to_string());
        let url = downloader
            .artifact_url(&artifact("DDW_Common", b""))
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://magnet.supercollider.online/dl/ddw-common/1.2.3/ddw-common-1.2.3-quark-any-any.tar.gz"
        );

        let downloader =
            PackageDownloader::new(PathBuf::new(), "https://foo/{name}-{arch}".to_string());
        assert!(downloader.artifact_url(&artifact("foo", b"")).is_err());
    }

    #[test]
    fn test_cache_path_rejects_invalid_checksum() {
        let downloader = PackageDownloader::new(PathBuf::new(), OFFICIAL_TEMPLATE.to_string());
        let mut artifact = artifact("foo", b"foo");
        assert!(downloader.cache_path(&artifact).is_ok());
        for sha256 in ["../../etc/passwd", "abc", &"g".repeat(64)] {
            artifact.sha256 = sha256.to_string();
            assert!(downloader.cache_path(&artifact).is_err(), "{sha256}");
        }
    }

    #[tokio::test]
    async fn test_download_and_cache() {
        let (mirror, template) = local_mirror(&[("foo", b"foo content")]);
        let cache = tempfile::tempdir().unwrap();
        let downloader = PackageDownloader::new(cache.path().to_path_buf(), template);
        let artifact = artifact("foo", b"foo content");

        let path = downloader
            .download(&artifact, &ProgressBar::hidden())
            .await
            .unwrap();
        assert_eq!(path, downloader.cache_path(&artifact).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"foo content");

        // a second download is served from the cache
        mirror.close().unwrap();
        let path = downloader
            .download(&artifact, &ProgressBar::hidden())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"foo content");
    }

    #[tokio::test]
    async fn test_checksum_mismatch() {
        let (_mirror, template) = local_mirror(&[("foo", b"tampered")]);
        let cache = tempfile::tempdir().unwrap();
        let downloader = PackageDownloader::new(cache.path().to_path_buf(), template);

        let err = downloader
            .download(&artifact("foo", b"original"), &ProgressBar::hidden())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Checksum mismatch"), "{err:#}");
        // neither the file nor a partial download remains in the cache
        assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn test_corrupted_cache_is_replaced() {
        let (_mirror, template) = local_mirror(&[("foo", b"foo content")]);
        let cache = tempfile::tempdir().unwrap();
        let downloader = PackageDownloader::new(cache.path().to_path_buf(), template);
        let artifact = artifact("foo", b"foo content");
        std::fs::write(downloader.cache_path(&artifact).unwrap(), b"corrupted").unwrap();

        let path = downloader
            .download(&artifact, &ProgressBar::hidden())
            .await
            .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"foo content");
    }

    #[tokio::test]
    async fn test_download_all() {
        let files: [(&str, &[u8]); 3] = [("alpha", b"a"), ("beta", b"b"), ("gamma", b"c")];
        let (_mirror, template) = local_mirror(&files);
        let cache = tempfile::tempdir().unwrap();
        let downloader = PackageDownloader::new(cache.path().to_path_buf(), template);
        let artifacts: Vec<Artifact> = files
            .iter()
            .map(|(name, content)| artifact(name, content))
            .collect();

        let paths = downloader.download_all(&artifacts, 2).await.unwrap();
        for (path, (_, content)) in paths.iter().zip(files) {
            assert_eq!(std::fs::read(path).unwrap(), content);
        }

        // all failures get reported
        let missing = [artifact("missing", b"x"), artifact("absent", b"y")];
        let err = downloader
            .download_all(&missing, 2)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing") && err.contains("absent"), "{err}");
    }

    /// serves a single HTTP response for every request
    async fn http_server(status: &'static str, body: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = vec![0; 4096];
                let _ = socket.read(&mut request).await;
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(body).await;
            }
        });
        format!("http://{address}/dl/{{name}}-{{version}}.tar.gz")
    }

    #[tokio::test]
    async fn test_http_download() {
        let template = http_server("200 OK", b"served via http").await;
        let cache = tempfile::tempdir().unwrap();
        let downloader = PackageDownloader::new(cache.path().to_path_buf(), template);
        let path = downloader
            .download(&artifact("foo", b"served via http"), &ProgressBar::hidden())
            .await
            .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"served via http");
    }

    #[tokio::test]
    async fn test_http_gone() {
        let template = http_server("410 Gone", b"").await;
        let cache = tempfile::tempdir().unwrap();
        let downloader = PackageDownloader::new(cache.path().to_path_buf(), template);
        let err = downloader
            .download(&artifact("foo", b"foo"), &ProgressBar::hidden())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("deleted") && err.contains("magnet update foo"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn test_http_not_found() {
        let template = http_server("404 Not Found", b"").await;
        let cache = tempfile::tempdir().unwrap();
        let downloader = PackageDownloader::new(cache.path().to_path_buf(), template);
        let err = downloader
            .download(&artifact("foo", b"foo"), &ProgressBar::hidden())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("404"), "{err:#}");
    }
}
