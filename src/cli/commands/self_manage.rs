use clap::{Parser, Subcommand};

use crate::cli::{CliContext, MagnetSubCommand};

/// The install scripts of the dist releases on GitHub (`.github/workflows/release.yml`)
pub const INSTALL_SH: &str =
    "curl --proto '=https' --tlsv1.2 -LsSf https://github.com/capital-G/magnet_cli/releases/latest/download/magnet-installer.sh | sh";
pub const INSTALL_PS1: &str =
    "powershell -ExecutionPolicy Bypass -c \"irm https://github.com/capital-G/magnet_cli/releases/latest/download/magnet-installer.ps1 | iex\"";

/// Manages the magnet executable itself
#[derive(Parser, Debug)]
pub struct SelfArgs {
    #[command(subcommand)]
    command: SelfCommand,
}

#[derive(Subcommand, Debug)]
enum SelfCommand {
    /// Replaces magnet with its newest release on GitHub
    Update(SelfUpdateArgs),
}

/// Replaces magnet with its newest release on GitHub
#[derive(Parser, Debug)]
pub struct SelfUpdateArgs {}

impl MagnetSubCommand for SelfArgs {
    async fn run(self, cli_context: &CliContext) -> anyhow::Result<()> {
        match self.command {
            SelfCommand::Update(args) => args.run(cli_context).await,
        }
    }
}

#[cfg(not(feature = "self-update"))]
impl MagnetSubCommand for SelfUpdateArgs {
    async fn run(self, _cli_context: &CliContext) -> anyhow::Result<()> {
        Err(magnet::error::HintedError {
            message: "This magnet was built without self-update".to_string(),
            hints: vec!["update magnet with the package manager you installed it with".to_string()],
        }
        .into())
    }
}

/// A GitHub token for the release lookups, e.g. against the rate limit in CI
#[cfg(feature = "self-update")]
pub const MAGNET_GITHUB_TOKEN_ENV: &str = "MAGNET_GITHUB_TOKEN";

#[cfg(feature = "self-update")]
fn install_script_hints() -> Vec<String> {
    vec![
        format!("install magnet with its install script: {INSTALL_SH}"),
        format!("on Windows: {INSTALL_PS1}"),
        "or update magnet the way you installed it".to_string(),
    ]
}

#[cfg(feature = "self-update")]
impl MagnetSubCommand for SelfUpdateArgs {
    async fn run(self, _cli_context: &CliContext) -> anyhow::Result<()> {
        use anyhow::Context;
        use axoupdater::{AxoUpdater, AxoupdateError};
        use magnet::{config::constants::MAGNET_CLI_VERSION, error::HintedError};

        let mut updater = AxoUpdater::new_for("magnet");
        if let Ok(token) = std::env::var(MAGNET_GITHUB_TOKEN_ENV) {
            updater.set_github_token(&token);
        }

        // the receipt (`~/.config/magnet/magnet-receipt.json`) is written by the install script
        // and tells where magnet lives and where its releases come from
        match updater.load_receipt() {
            Ok(_) => {}
            Err(AxoupdateError::NoReceipt { .. } | AxoupdateError::ReceiptLoadFailed { .. }) => {
                return Err(HintedError {
                    message: "This magnet wasn't installed by its install script, so it can't update itself".to_string(),
                    hints: install_script_hints(),
                }
                .into());
            }
            Err(err) => return Err(err).context("Could not read how magnet was installed"),
        }
        if !updater
            .check_receipt_is_for_this_executable()
            .context("Could not compare this magnet with its install receipt")?
        {
            let exe = std::env::current_exe()?;
            return Err(HintedError {
                message: format!(
                    "This magnet ({}) isn't the one the install script installed, so it can't update itself",
                    exe.display()
                ),
                hints: install_script_hints(),
            }
            .into());
        }

        updater.disable_installer_output();
        match updater
            .run()
            .await
            .context("Could not update magnet from its GitHub releases")?
        {
            Some(result) => println!(
                "Updated magnet from {} to {}",
                result
                    .old_version
                    .map(|version| version.to_string())
                    .unwrap_or_else(|| MAGNET_CLI_VERSION.to_string()),
                result.new_version
            ),
            None => println!("magnet {MAGNET_CLI_VERSION} is up to date"),
        }
        Ok(())
    }
}
