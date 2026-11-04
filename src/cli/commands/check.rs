use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context};
use clap::Parser;
use magnet::{
    http_client::basic::{AuthToken, MagnetClient},
    models::{package::ArtifactKind, project::Project, project_manifest::MagnetManifestV1},
    packaging::class_check::{class_summary, conflict_warnings},
};

use crate::cli::{commands::project::TargetArgs, CliContext, MagnetSubCommand};

/// Packages the current magnet project like `magnet package` and asks the magnet server for
/// naming conflicts of its classes with the current versions of other packages (C7.3). Nothing
/// is stored on the server.
#[derive(Parser, Debug)]
pub struct CheckArgs {
    /// Path of the project, the current directory by default
    #[arg(long)]
    path: Option<PathBuf>,

    /// Package the source from git although its files have uncommitted changes
    #[arg(long)]
    allow_dirty: bool,

    /// List every class with its methods, not only the count per file
    #[arg(long)]
    list: bool,

    /// Fail if a class or method conflicts with another package, e.g. in CI
    #[arg(long)]
    deny_conflicts: bool,

    #[command(flatten)]
    target: TargetArgs,
}

impl MagnetSubCommand for CheckArgs {
    async fn run(self, cli_context: &CliContext) -> anyhow::Result<()> {
        let project_path = match self.path {
            Some(path) => path,
            None => env::current_dir()?,
        };
        let mut project = Project::open(&project_path)?;
        project.set_target(self.target.lazy());
        let mut out = io::stdout();
        let Some(quark) = package(&project, self.allow_dirty, &mut out)? else {
            return Ok(());
        };
        // after packaging, so its offline errors come first
        let client = cli_context.authenticated_client().await?;
        check_quark(
            &client,
            project.manifest(),
            &quark,
            self.list,
            self.deny_conflicts,
            &mut out,
        )
        .await
    }
}

/// Packages the project into `dist/` like `magnet package` and returns its quark archive, if
/// it has one
fn package(
    project: &Project,
    allow_dirty: bool,
    out: &mut impl Write,
) -> anyhow::Result<Option<PathBuf>> {
    let archives = project.create_packages(allow_dirty)?;
    for archive in &archives {
        let path = archive
            .path
            .strip_prefix(project.root())
            .unwrap_or(&archive.path);
        writeln!(
            out,
            "Packaged {} ({} files)",
            path.display(),
            archive.files.len()
        )?;
    }
    let quark = archives
        .into_iter()
        .find(|archive| archive.kind == ArtifactKind::Quark)
        .map(|archive| archive.path);
    if quark.is_none() {
        writeln!(
            out,
            "The project has no [artifact.quark], so there are no classes to check."
        )?;
    }
    Ok(quark)
}

/// Sends the quark archive to the server's class check and prints its classes, the files the
/// server couldn't parse and the naming conflicts, which only fail with `deny_conflicts`
async fn check_quark(
    client: &MagnetClient<AuthToken>,
    manifest: &MagnetManifestV1,
    quark: &Path,
    list: bool,
    deny_conflicts: bool,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let name = MagnetManifestV1::normalize_name(&manifest.package.name);
    let content = fs::read(quark).with_context(|| format!("Could not read {quark:?}"))?;
    let Some(check) = client.check_classes(&name, content).await? else {
        bail!(
            "The magnet server at {} can't check classes yet (it has no classes API)",
            client.base_url
        );
    };

    writeln!(out)?;
    if check.classes.is_empty() {
        writeln!(out, "The quark contains no classes.")?;
    } else {
        writeln!(out, "Classes of the quark:")?;
        for line in class_summary(&check.classes, list) {
            writeln!(out, "  {line}")?;
        }
    }
    if !check.errors.is_empty() {
        writeln!(
            out,
            "\nFiles the server could not parse (their classes aren't checked):"
        )?;
        for error in &check.errors {
            writeln!(out, "  {}: {}", error.path, error.message)?;
        }
    }
    if check.conflicts.is_empty() {
        writeln!(
            out,
            "\nNo naming conflicts with the current versions of other packages."
        )?;
        return Ok(());
    }
    writeln!(
        out,
        "\nNaming conflicts with the current versions of other packages:"
    )?;
    for warning in conflict_warnings(&check.conflicts) {
        writeln!(out, "  {warning}")?;
    }
    if deny_conflicts {
        let n = check.conflicts.len();
        bail!(
            "{name} conflicts with {n} other package{} (--deny-conflicts)",
            if n == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use wiremock::{
        matchers::{body_bytes, header, method, path, query_param},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;

    const MANIFEST: &str = "magnet-version = 1\n[package]\nname = \"Foo_Bar\"\nversion = \"1.2.0\"\n[artifact.source]\nfrom = \"paths\"\npaths = [\"*.sc\"]\n[artifact.quark]\npaths = [\"*.sc\"]\n[dependencies]\n";

    /// a project whose source is taken from paths, so it needs no git repository
    fn project(manifest: &str) -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("magnet.toml"), manifest).unwrap();
        fs::write(
            dir.path().join("Foo.sc"),
            "Foo {\n\t*new { ^super.new }\n}\n",
        )
        .unwrap();
        let project = Project::open(dir.path()).unwrap();
        (dir, project)
    }

    /// packages the project, then answers the check of its quark archive with `response`
    async fn server(
        project: &Project,
        response: ResponseTemplate,
    ) -> (MockServer, PathBuf, String) {
        let mut out = vec![];
        let quark = package(project, false, &mut out).unwrap().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/classes/check/"))
            .and(query_param("package", "foo-bar"))
            .and(header("authorization", "Bearer token"))
            .and(header("content-type", "application/gzip"))
            .and(body_bytes(fs::read(&quark).unwrap()))
            .respond_with(response)
            .mount(&server)
            .await;
        (server, quark, String::from_utf8(out).unwrap())
    }

    fn conflicts() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "classes": [{
                "name": "Foo", "superclass": "Object", "extension": false, "path": "Foo.sc",
                "line": 1, "platform": "", "methods": [{"name": "new", "class_method": true}],
            }],
            "errors": [{"path": "Broken.sc", "message": "unexpected '}'"}],
            "conflicts": [{
                "package": "beta-lib", "version": "1.0.0", "classes": ["Foo"], "methods": [],
            }],
        }))
    }

    fn client(server: &MockServer) -> MagnetClient<AuthToken> {
        MagnetClient::new(server.uri().parse::<url::Url>().unwrap())
            .with_state(AuthToken::Env("token".to_string()))
    }

    async fn check(
        server: &MockServer,
        project: &Project,
        quark: &Path,
        list: bool,
        deny_conflicts: bool,
    ) -> (anyhow::Result<()>, String) {
        let mut out = vec![];
        let result = check_quark(
            &client(server),
            project.manifest(),
            quark,
            list,
            deny_conflicts,
            &mut out,
        )
        .await;
        (result, String::from_utf8(out).unwrap())
    }

    #[tokio::test]
    async fn test_conflicts_are_warnings() {
        let (_dir, project) = project(MANIFEST);
        let (server, quark, packaged) = server(&project, conflicts()).await;
        let expected = format!(
            "Packaged {} (2 files)",
            Path::new("dist")
                .join("foo-bar-1.2.0-quark-any-any.tar.gz")
                .display()
        );
        assert!(packaged.contains(&expected), "{packaged}");

        let (result, out) = check(&server, &project, &quark, false, false).await;
        result.unwrap();
        for expected in [
            "Classes of the quark:\n  Foo.sc: 1 class\n",
            "  Broken.sc: unexpected '}'",
            "  beta-lib 1.0.0 also defines class Foo, so sclang can't compile both packages together",
        ] {
            assert!(out.contains(expected), "{expected}:\n{out}");
        }
        assert!(!out.contains("*new"), "{out}");

        let (result, out) = check(&server, &project, &quark, true, false).await;
        result.unwrap();
        assert!(
            out.contains("  Foo.sc: 1 class\n    Foo : Object (line 1): *new\n"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn test_deny_conflicts() {
        let (_dir, project) = project(MANIFEST);
        let (server, quark, _) = server(&project, conflicts()).await;

        let (result, out) = check(&server, &project, &quark, false, true).await;
        assert!(
            out.contains("beta-lib 1.0.0 also defines class Foo"),
            "{out}"
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "foo-bar conflicts with 1 other package (--deny-conflicts)"
        );
    }

    #[tokio::test]
    async fn test_no_conflicts() {
        let (_dir, project) = project(MANIFEST);
        let (server, quark, _) = server(
            &project,
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"classes": [], "errors": [], "conflicts": []})),
        )
        .await;

        let (result, out) = check(&server, &project, &quark, false, true).await;
        result.unwrap();
        assert!(out.contains("The quark contains no classes."), "{out}");
        assert!(out.contains("No naming conflicts"), "{out}");
    }

    /// an older server without the classes API
    #[tokio::test]
    async fn test_unsupported_server() {
        let (_dir, project) = project(MANIFEST);
        let mut out = vec![];
        let quark = package(&project, false, &mut out).unwrap().unwrap();
        let server = MockServer::start().await;

        let (result, _) = check(&server, &project, &quark, false, false).await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("can't check classes yet"), "{err}");
    }

    #[test]
    fn test_no_quark() {
        let (_dir, project) = project(&MANIFEST.replace(
            "[artifact.quark]\npaths = [\"*.sc\"]\n",
            "[artifact.lepton]\nplatform-paths = { linux_x86_64 = [\"*.sc\"] }\n",
        ));
        let mut project = project;
        project.set_target(magnet::target::LazyTarget::new(
            Some("linux_x86_64".to_string()),
            Some("api3_0".to_string()),
        ));
        let mut out = vec![];
        assert_eq!(package(&project, false, &mut out).unwrap(), None);
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("no classes to check"), "{out}");
    }
}
