use std::io::{self, Write};

use anyhow::bail;
use clap::{ArgGroup, Parser};
use magnet::{
    http_client::basic::{ApplyAuth, MagnetClient},
    packaging::class_check::method_label,
};

use crate::cli::{CliContext, MagnetSubCommand};

/// Searches the classes and methods of the current versions of the packages on the magnet
/// server (C7.4), e.g. after sclang's "Class not defined" or "Message not understood". Needs no
/// login.
#[derive(Parser, Debug)]
#[command(group(ArgGroup::new("query").required(true).args(["class", "method"])))]
pub struct SearchArgs {
    /// Classes and class extensions whose name starts with this, ignoring the case
    #[arg(long, value_name = "NAME")]
    class: Option<String>,

    /// Methods whose name starts with this, ignoring the case
    #[arg(long, value_name = "NAME")]
    method: Option<String>,
}

impl MagnetSubCommand for SearchArgs {
    async fn run(self, cli_context: &CliContext) -> anyhow::Result<()> {
        let client = cli_context.http_client()?;
        let query = match (self.class, self.method) {
            (Some(name), _) => Query::Class(name),
            (None, Some(name)) => Query::Method(name),
            (None, None) => unreachable!("clap requires --class or --method"),
        };
        search(&client, &query, &mut io::stdout()).await
    }
}

#[derive(Debug)]
enum Query {
    Class(String),
    Method(String),
}

/// Prints the hits as a table (name, package, version, file), a note if the server left some
/// out, and how to add a package
async fn search<State: ApplyAuth>(
    client: &MagnetClient<State>,
    query: &Query,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let (q, what) = match query {
        Query::Class(q) => (q, "class"),
        Query::Method(q) => (q, "method"),
    };
    let Some(found) = client.search_classes(q).await? else {
        bail!(
            "The magnet server at {} can't search classes yet (it has no classes API)",
            client.base_url
        );
    };
    // [name, package, version, file]
    let rows: Vec<[String; 4]> = match query {
        Query::Class(_) => found
            .classes
            .into_iter()
            .map(|hit| {
                let name = match hit.extension {
                    true => format!("+ {}", hit.name),
                    false => hit.name,
                };
                [name, hit.package, hit.version, hit.path]
            })
            .collect(),
        Query::Method(_) => found
            .methods
            .into_iter()
            .map(|hit| {
                let label = method_label(&hit.class_name, &hit.name, hit.class_method);
                let name = match hit.extension {
                    true => format!("+ {label}"),
                    false => label,
                };
                [name, hit.package, hit.version, hit.path]
            })
            .collect(),
    };
    if rows.is_empty() {
        writeln!(
            out,
            "No {what} starting with `{q}` in the current versions of the packages on {}.",
            client.base_url
        )?;
        return Ok(());
    }

    let header = [what, "package", "version", "file"].map(String::from);
    let mut widths = [0; 4];
    for row in std::iter::once(&header).chain(&rows) {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    for row in std::iter::once(&header).chain(&rows) {
        let line = format!(
            "{:w0$}  {:w1$}  {:w2$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
        );
        writeln!(out, "{}", line.trim_end())?;
    }
    if found.truncated {
        writeln!(
            out,
            "\nThe server only returns the first 50 matches, type more of the name to narrow the search."
        )?;
    }
    let mut packages: Vec<&str> = rows.iter().map(|row| row[1].as_str()).collect();
    packages.sort();
    packages.dedup();
    match packages.as_slice() {
        [package] => writeln!(out, "\n`magnet add {package}` adds it to the project.")?,
        _ => writeln!(
            out,
            "\n`magnet add <package>` adds one of them to the project."
        )?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use magnet::http_client::basic::NoAuth;
    use wiremock::{
        matchers::{method, path, query_param},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;

    async fn server(q: &str, response: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/classes/"))
            .and(query_param("q", q))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .expect(1)
            .mount(&server)
            .await;
        server
    }

    fn client(server: &MockServer) -> MagnetClient<NoAuth> {
        MagnetClient::new(server.uri().parse::<url::Url>().unwrap())
    }

    async fn output(server: &MockServer, query: Query) -> anyhow::Result<String> {
        let mut out = vec![];
        search(&client(server), &query, &mut out).await?;
        Ok(String::from_utf8(out).unwrap())
    }

    fn hits() -> serde_json::Value {
        serde_json::json!({
            "classes": [
                {"name": "Pbind", "extension": false, "package": "alpha", "version": "1.2.0", "path": "Pbind.sc"},
                {"name": "Pbindef", "extension": true, "package": "beta-lib", "version": "0.1.0", "path": "ext/Pbindef.sc"},
            ],
            "methods": [
                {"name": "asJSON", "class": "Object", "class_method": false, "extension": true,
                 "package": "json", "version": "2.0.0", "path": "Object+json.sc"},
                {"name": "asJSONString", "class": "JSON", "class_method": true, "extension": false,
                 "package": "json", "version": "2.0.0", "path": "JSON.sc"},
            ],
            "truncated": false,
        })
    }

    #[tokio::test]
    async fn test_classes() {
        let server = server("Pbind", hits()).await;
        assert_eq!(
            output(&server, Query::Class("Pbind".to_string()))
                .await
                .unwrap(),
            "class      package   version  file
Pbind      alpha     1.2.0    Pbind.sc
+ Pbindef  beta-lib  0.1.0    ext/Pbindef.sc

`magnet add <package>` adds one of them to the project.
"
        );
    }

    #[tokio::test]
    async fn test_methods() {
        let server = server("asJSON", hits()).await;
        assert_eq!(
            output(&server, Query::Method("asJSON".to_string()))
                .await
                .unwrap(),
            "method              package  version  file
+ Object:-asJSON    json     2.0.0    Object+json.sc
JSON:*asJSONString  json     2.0.0    JSON.sc

`magnet add json` adds it to the project.
"
        );
    }

    #[tokio::test]
    async fn test_no_hits() {
        // classes were found, but no methods
        let mut response = hits();
        response["methods"] = serde_json::json!([]);
        let server = server("Pb", response).await;
        let out = output(&server, Query::Method("Pb".to_string()))
            .await
            .unwrap();
        assert!(
            out.starts_with("No method starting with `Pb` in the current versions"),
            "{out}"
        );
        assert!(!out.contains("magnet add"), "{out}");
    }

    #[tokio::test]
    async fn test_truncated() {
        let mut response = hits();
        response["truncated"] = serde_json::json!(true);
        let server = server("P", response).await;
        let out = output(&server, Query::Class("P".to_string()))
            .await
            .unwrap();
        assert!(out.contains("only returns the first 50 matches"), "{out}");
    }

    /// an older server without the classes API
    #[tokio::test]
    async fn test_unsupported_server() {
        let server = MockServer::start().await;
        let err = output(&server, Query::Class("Foo".to_string()))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("can't search classes yet"),
            "{err}"
        );
    }
}
