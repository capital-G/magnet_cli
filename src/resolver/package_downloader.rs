use std::{env, path::PathBuf};

use futures_util::{stream, StreamExt};
use indicatif::ProgressBar;
use tokio::io::AsyncWriteExt;

use crate::{
    git_client::models::GitDependency,
    http_client::basic::{MagnetClient, NoAuth},
};

trait PackageDownloader {
    async fn download_package(
        &self,
        dependency: &GitDependency,
        target_dir: PathBuf,
        progress_bar: ProgressBar,
    ) -> anyhow::Result<()>;
}

struct CacheDownloader {
    location: PathBuf,
}

struct HttpDownloader {
    http_client: MagnetClient<NoAuth>,
}

impl PackageDownloader for MagnetClient<NoAuth> {
    async fn download_package(
        &self,
        dependency: &GitDependency,
        target_dir: PathBuf,
        progress_bar: ProgressBar,
    ) -> anyhow::Result<()> {
        let target_path = target_dir.join("foo.tar.gz");
        let res = self.client.get("url").send().await?;
        let download_size = res.content_length().ok_or(0).unwrap();

        progress_bar.set_length(download_size);

        progress_bar.set_message(format!("Downloalding {}: ", dependency.name));

        let mut file = tokio::fs::File::create(target_path).await?;
        let mut stream = res.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            progress_bar.inc(chunk.len() as u64);
        }

        progress_bar.finish_with_message("Downloaded!");

        Ok(())
    }
}

async fn downolad_dependencies(
    dependencies: Vec<GitDependency>,
    clients: Vec<impl PackageDownloader>,
    _num_paralell: u32,
) -> anyhow::Result<()> {
    let multi_pb = indicatif::MultiProgress::new();

    let _ = stream::iter(dependencies.into_iter().map(async |dependency| {
        let progress_bar = multi_pb.add(ProgressBar::new(0));

        for client in &clients {
            let target_dir = env::current_dir().unwrap().join("tmp");
            match client
                .download_package(&dependency, target_dir, progress_bar)
                .await
            {
                Ok(()) => break,
                Err(_) => todo!(),
            }
        }
    }));

    Ok(())
}
