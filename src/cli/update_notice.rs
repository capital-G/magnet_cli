//! The notice about a new magnet release: at most once a day, a command asks GitHub for the newest
//! release while it runs, waits at most [WAIT] for the answer at its end and then names the newer
//! version on stderr. Only a magnet installed by its install script checks (it needs the install
//! receipt of [axoupdater]), and nothing here ever fails a command.

/// Turns the check off, e.g. `MAGNET_NO_UPDATE_CHECK=1`
pub const MAGNET_NO_UPDATE_CHECK_ENV: &str = "MAGNET_NO_UPDATE_CHECK";

/// A started check, see [UpdateNotice::start]
pub struct UpdateNotice {
    #[cfg(feature = "self-update")]
    check: Option<tokio::task::JoinHandle<Option<semver::Version>>>,
}

#[cfg(not(feature = "self-update"))]
impl UpdateNotice {
    pub fn start(_skip: bool) -> Self {
        UpdateNotice {}
    }

    pub async fn finish(self) {}
}

#[cfg(feature = "self-update")]
mod enabled {
    use super::{UpdateNotice, MAGNET_NO_UPDATE_CHECK_ENV};
    use magnet::config::constants::{magnet_home, MAGNET_CLI_VERSION};
    use serde::{Deserialize, Serialize};
    use std::{
        io::IsTerminal,
        path::PathBuf,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    /// how long a command waits for the check at its end
    pub const WAIT: Duration = Duration::from_secs(1);
    /// how often magnet asks GitHub
    pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
    /// the last check within the magnet home directory
    pub const STATE_FILE: &str = "update_check.json";

    /// The last completed check
    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    pub struct UpdateCheckState {
        /// unix seconds
        pub checked_at: u64,
        pub latest_version: String,
    }

    impl UpdateNotice {
        /// Starts the check in the background if it's due, unless `skip` (e.g. for
        /// `magnet self update`), `MAGNET_NO_UPDATE_CHECK` is set or stderr isn't a terminal
        pub fn start(skip: bool) -> Self {
            let enabled = !skip
                && !disabled_by_env(std::env::var(MAGNET_NO_UPDATE_CHECK_ENV).ok().as_deref())
                && std::io::stderr().is_terminal();
            let due = enabled
                && state_path()
                    .map(|path| is_due(read_state(&path).as_ref(), now()))
                    .unwrap_or(false);
            UpdateNotice {
                check: due.then(|| tokio::spawn(latest_release())),
            }
        }

        /// Waits at most [WAIT] for the check, stores its result and prints the notice if
        /// there's a newer release
        pub async fn finish(self) {
            let Some(check) = self.check else { return };
            let Ok(Ok(Some(latest))) = tokio::time::timeout(WAIT, check).await else {
                return;
            };
            if let Some(path) = state_path() {
                write_state(
                    &path,
                    &UpdateCheckState {
                        checked_at: now(),
                        latest_version: latest.to_string(),
                    },
                );
            }
            if let Some(notice) = notice(MAGNET_CLI_VERSION, &latest) {
                let yellow = anstyle::Style::new()
                    .bold()
                    .fg_color(Some(anstyle::AnsiColor::Yellow.into()));
                anstream::eprintln!("\n{yellow}notice:{yellow:#} {notice}");
            }
        }
    }

    /// A set variable turns the check off, except for an empty value or `0`
    pub fn disabled_by_env(value: Option<&str>) -> bool {
        value.is_some_and(|value| !value.is_empty() && value != "0")
    }

    /// Without a completed check, or once [CHECK_INTERVAL] has passed since it
    pub fn is_due(state: Option<&UpdateCheckState>, now: u64) -> bool {
        state.is_none_or(|state| now.saturating_sub(state.checked_at) >= CHECK_INTERVAL.as_secs())
    }

    /// The notice if `latest` is newer than `current`
    pub fn notice(current: &str, latest: &semver::Version) -> Option<String> {
        let current = semver::Version::parse(current).ok()?;
        (latest > &current).then(|| {
            format!("magnet {latest} is available (you have {current}), run `magnet self update`")
        })
    }

    /// The newest release, if this magnet was installed by its install script
    async fn latest_release() -> Option<semver::Version> {
        let mut updater = axoupdater::AxoUpdater::new_for("magnet");
        if let Ok(token) = std::env::var(crate::cli::commands::self_manage::MAGNET_GITHUB_TOKEN_ENV)
        {
            updater.set_github_token(&token);
        }
        if let Err(err) = updater.load_receipt() {
            tracing::debug!("No update check, no install receipt: {err}");
            return None;
        }
        if !updater
            .check_receipt_is_for_this_executable()
            .unwrap_or(false)
        {
            tracing::debug!("No update check, the install receipt is for another magnet");
            return None;
        }
        match updater.query_new_version().await {
            Ok(latest) => semver::Version::parse(&latest?.to_string()).ok(),
            Err(err) => {
                tracing::debug!("The update check failed: {err}");
                None
            }
        }
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0)
    }

    fn state_path() -> Option<PathBuf> {
        Some(magnet_home().ok()?.join(STATE_FILE))
    }

    pub fn read_state(path: &std::path::Path) -> Option<UpdateCheckState> {
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    pub fn write_state(path: &std::path::Path, state: &UpdateCheckState) {
        let Ok(json) = serde_json::to_vec(state) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(err) = std::fs::write(path, json) {
            tracing::debug!(
                "Could not store the update check in {}: {err}",
                path.display()
            );
        }
    }
}

#[cfg(all(test, feature = "self-update"))]
mod tests {
    use super::enabled::*;

    #[test]
    fn test_is_due_once_a_day() {
        let state = UpdateCheckState {
            checked_at: 1_000_000,
            latest_version: "0.1.0".to_string(),
        };
        assert!(is_due(None, 1_000_000));
        assert!(!is_due(Some(&state), 1_000_000 + 60));
        assert!(is_due(Some(&state), 1_000_000 + CHECK_INTERVAL.as_secs()));
        // a clock moved backwards doesn't check on every run
        assert!(!is_due(Some(&state), 0));
    }

    #[test]
    fn test_notice_only_for_newer_versions() {
        let latest = semver::Version::parse("0.2.0").unwrap();
        assert_eq!(
            notice("0.1.0", &latest).as_deref(),
            Some("magnet 0.2.0 is available (you have 0.1.0), run `magnet self update`")
        );
        assert_eq!(notice("0.2.0", &latest), None);
        assert_eq!(notice("0.3.0", &latest), None);
    }

    #[test]
    fn test_disabled_by_env() {
        assert!(!disabled_by_env(None));
        assert!(!disabled_by_env(Some("")));
        assert!(!disabled_by_env(Some("0")));
        assert!(disabled_by_env(Some("1")));
        assert!(disabled_by_env(Some("true")));
    }

    #[test]
    fn test_state_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("home").join(STATE_FILE);
        assert_eq!(read_state(&path), None);
        let state = UpdateCheckState {
            checked_at: 42,
            latest_version: "0.2.0".to_string(),
        };
        write_state(&path, &state);
        assert_eq!(read_state(&path), Some(state));
    }
}
