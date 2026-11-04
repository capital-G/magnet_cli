//! The classes API of the magnet server (server M5): searching classes and methods, the naming
//! conflicts of a package and checking a quark artifact before publishing. Formats in
//! `../magnet_server/implementation_plan.md` "Contract (M5)"; unknown response fields are
//! ignored, so the server can add some.
//!
//! sclang has one flat namespace: a class defined by two packages doesn't compile, and of two
//! extensions adding the same method one silently replaces the other.

use reqwest::{Response, StatusCode};
use serde::{de::DeserializeOwned, Deserialize};
use tracing::instrument;

use crate::http_client::{
    basic::{ApplyAuth, AuthToken, MagnetClient},
    publish::refusal,
};

/// A class or class extension found by [`MagnetClient::search_classes`]
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ClassHit {
    pub name: String,
    /// `+ Foo { … }` instead of `Foo { … }`
    pub extension: bool,
    /// the normalised name
    pub package: String,
    /// the package's current version (newest final, non-yanked)
    pub version: String,
    /// the file in the quark artifact
    pub path: String,
}

/// A method found by [`MagnetClient::search_classes`]
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MethodHit {
    pub name: String,
    /// the class the method belongs to (or extends)
    #[serde(rename = "class")]
    pub class_name: String,
    /// `*name` instead of `name`
    pub class_method: bool,
    /// added by a class extension
    pub extension: bool,
    pub package: String,
    pub version: String,
    pub path: String,
}

/// Classes and methods whose names start with the query, ignoring the case, exact matches first
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ClassSearch {
    #[serde(default)]
    pub classes: Vec<ClassHit>,
    #[serde(default)]
    pub methods: Vec<MethodHit>,
    /// more than 50 classes or methods matched, the rest was left out
    #[serde(default)]
    pub truncated: bool,
}

/// A method added by extensions in both packages
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ConflictingMethod {
    #[serde(rename = "class")]
    pub class_name: String,
    pub name: String,
    pub class_method: bool,
}

/// The clashes with the current version of another package
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PackageConflict {
    /// the other package, normalised
    pub package: String,
    pub version: String,
    /// classes both define (sclang refuses to compile them together)
    #[serde(default)]
    pub classes: Vec<String>,
    /// methods extensions of both add (one silently replaces the other)
    #[serde(default)]
    pub methods: Vec<ConflictingMethod>,
}

/// The conflicts of a package's current version with the other packages
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PackageConflicts {
    pub package: String,
    /// `None` if the package has no current version
    pub version: Option<String>,
    #[serde(default)]
    pub conflicts: Vec<PackageConflict>,
}

/// A method of a checked class
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CheckedMethod {
    pub name: String,
    pub class_method: bool,
}

/// A class (or class extension) of a checked quark artifact
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CheckedClass {
    pub name: String,
    /// empty for extensions and `Object`
    #[serde(default)]
    pub superclass: String,
    pub extension: bool,
    pub path: String,
    pub line: u64,
    /// the platform folder (`osx`, `linux`, …) the file is in, empty if none
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub methods: Vec<CheckedMethod>,
}

/// A `.sc` file the server couldn't parse
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FileError {
    pub path: String,
    pub message: String,
}

/// The classes of a quark artifact and their clashes with the other packages
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ClassCheck {
    #[serde(default)]
    pub classes: Vec<CheckedClass>,
    #[serde(default)]
    pub errors: Vec<FileError>,
    #[serde(default)]
    pub conflicts: Vec<PackageConflict>,
}

impl<State: ApplyAuth> MagnetClient<State> {
    /// Classes and methods of the current versions whose names start with `q`. `None` if the
    /// server doesn't support it (an older server answers `404`). Needs no login.
    #[instrument(skip(self))]
    pub async fn search_classes(&self, q: &str) -> anyhow::Result<Option<ClassSearch>> {
        let mut url = self.api_url("api/v1/classes/")?;
        url.query_pairs_mut().append_pair("q", q);
        let response = self.client.get(url).send().await?;
        parse_public(response).await
    }

    /// The conflicts of the current version of the package `name` (any spelling). `None` for
    /// an unknown package (or one with only drafts) and for a server without the endpoint,
    /// both answer `404`. Needs no login.
    #[instrument(skip(self))]
    pub async fn package_conflicts(&self, name: &str) -> anyhow::Result<Option<PackageConflicts>> {
        let url = self.api_url(&format!("api/v1/packages/{name}/conflicts/"))?;
        let response = self.client.get(url).send().await?;
        parse_public(response).await
    }
}

impl MagnetClient<AuthToken> {
    /// Sends a quark artifact (`.tar.gz`) to the server, which reads its classes and reports
    /// their clashes with the current versions of the other packages; the package `name`'s own
    /// current version is left out. Nothing is stored. `None` if the server doesn't support it
    /// (`404`). Needs a token with the `publish` scope.
    #[instrument(skip(self, quark_archive))]
    pub async fn check_classes(
        &self,
        name: &str,
        quark_archive: Vec<u8>,
    ) -> anyhow::Result<Option<ClassCheck>> {
        let mut url = self.api_url("api/v1/classes/check/")?;
        url.query_pairs_mut().append_pair("package", name);
        // the whole body is sent with its Content-Length, like `upload_artifact`
        let response = self
            .client
            .post(url)
            .bearer_auth(self.state.access_token())
            .header(reqwest::header::CONTENT_TYPE, "application/gzip")
            .body(quark_archive)
            .send()
            .await?;
        match response.status() {
            StatusCode::NOT_FOUND => Ok(None),
            status if status.is_success() => Ok(Some(response.json().await?)),
            _ => Err(self.error(response).await),
        }
    }
}

/// The body of a request without login, `None` for a `404`
async fn parse_public<T: DeserializeOwned>(response: Response) -> anyhow::Result<Option<T>> {
    match response.status() {
        StatusCode::NOT_FOUND => Ok(None),
        status if status.is_success() => Ok(Some(response.json().await?)),
        _ => {
            let (status, detail) = refusal(response).await;
            Err(anyhow::anyhow!(
                "The server refused the request ({status}){detail}"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use wiremock::{
        matchers::{body_bytes, header, method, path, query_param},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;
    use crate::http_client::basic::NoAuth;

    fn client(server: &MockServer) -> MagnetClient<NoAuth> {
        MagnetClient::new(server.uri().parse::<url::Url>().unwrap())
    }

    fn auth_client(server: &MockServer) -> MagnetClient<AuthToken> {
        client(server).with_state(AuthToken::Env("token".to_string()))
    }

    /// the conflicts with one other package, as in the contract
    fn conflicts_json() -> serde_json::Value {
        serde_json::json!([{
            "package": "beta-lib",
            "version": "1.0.0",
            "classes": ["Foo"],
            "methods": [{"class": "String", "name": "shout", "class_method": false}],
        }])
    }

    fn expected_conflicts() -> Vec<PackageConflict> {
        vec![PackageConflict {
            package: "beta-lib".to_string(),
            version: "1.0.0".to_string(),
            classes: vec!["Foo".to_string()],
            methods: vec![ConflictingMethod {
                class_name: "String".to_string(),
                name: "shout".to_string(),
                class_method: false,
            }],
        }]
    }

    #[tokio::test]
    async fn test_search_classes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/classes/"))
            .and(query_param("q", "Foo Bar"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "classes": [{
                    "name": "Foo", "extension": false, "package": "alpha",
                    "version": "1.2.0", "path": "classes/Foo.sc",
                    "added_later": "is ignored",
                }],
                "methods": [{
                    "name": "fooBar", "class": "String", "class_method": true,
                    "extension": true, "package": "beta", "version": "0.1.0",
                    "path": "String+foo.sc",
                }],
                "truncated": true,
            })))
            .expect(1)
            .mount(&server)
            .await;

        let search = client(&server)
            .search_classes("Foo Bar")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            search,
            ClassSearch {
                classes: vec![ClassHit {
                    name: "Foo".to_string(),
                    extension: false,
                    package: "alpha".to_string(),
                    version: "1.2.0".to_string(),
                    path: "classes/Foo.sc".to_string(),
                }],
                methods: vec![MethodHit {
                    name: "fooBar".to_string(),
                    class_name: "String".to_string(),
                    class_method: true,
                    extension: true,
                    package: "beta".to_string(),
                    version: "0.1.0".to_string(),
                    path: "String+foo.sc".to_string(),
                }],
                truncated: true,
            }
        );
    }

    /// an API URL under a path prefix keeps it (C6.1)
    #[tokio::test]
    async fn test_search_classes_below_prefix() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/magnet/api/v1/classes/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"classes": [], "methods": [], "truncated": false}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let prefixed = |base: &str| {
            MagnetClient::new(
                format!("{}/{base}", server.uri())
                    .parse::<url::Url>()
                    .unwrap(),
            )
        };
        let search = prefixed("magnet")
            .search_classes("x")
            .await
            .unwrap()
            .unwrap();
        assert!(search.classes.is_empty() && !search.truncated);
        assert_eq!(
            prefixed("magnet/")
                .api_url("/api/v1/classes/")
                .unwrap()
                .path(),
            "/magnet/api/v1/classes/"
        );
    }

    #[tokio::test]
    async fn test_package_conflicts() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/packages/Foo_Bar/conflicts/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "package": "foo-bar",
                "version": "2.0.0",
                "conflicts": conflicts_json(),
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/packages/drafts-only/conflicts/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "package": "drafts-only",
                "version": null,
                "conflicts": [],
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/packages/unknown/conflicts/"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(serde_json::json!({"detail": "Not found."})),
            )
            .mount(&server)
            .await;

        let client = client(&server);
        assert_eq!(
            client.package_conflicts("Foo_Bar").await.unwrap(),
            Some(PackageConflicts {
                package: "foo-bar".to_string(),
                version: Some("2.0.0".to_string()),
                conflicts: expected_conflicts(),
            })
        );
        let none = client
            .package_conflicts("drafts-only")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(none.version, None);
        assert!(none.conflicts.is_empty());
        assert_eq!(client.package_conflicts("unknown").await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_check_classes() {
        let archive = b"not really gzip".to_vec();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/classes/check/"))
            .and(query_param("package", "foo-bar"))
            .and(header("authorization", "Bearer token"))
            .and(header("content-type", "application/gzip"))
            .and(header("content-length", archive.len().to_string().as_str()))
            .and(body_bytes(archive.clone()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "classes": [{
                    "name": "Foo", "superclass": "Object", "extension": false,
                    "path": "Foo.sc", "line": 3, "platform": "",
                    "methods": [{"name": "new", "class_method": true}],
                }],
                "errors": [{"path": "Broken.sc", "message": "unexpected '}'"}],
                "conflicts": conflicts_json(),
            })))
            .expect(1)
            .mount(&server)
            .await;

        let check = auth_client(&server)
            .check_classes("foo-bar", archive)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            check,
            ClassCheck {
                classes: vec![CheckedClass {
                    name: "Foo".to_string(),
                    superclass: "Object".to_string(),
                    extension: false,
                    path: "Foo.sc".to_string(),
                    line: 3,
                    platform: String::new(),
                    methods: vec![CheckedMethod {
                        name: "new".to_string(),
                        class_method: true,
                    }],
                }],
                errors: vec![FileError {
                    path: "Broken.sc".to_string(),
                    message: "unexpected '}'".to_string(),
                }],
                conflicts: expected_conflicts(),
            }
        );
    }

    /// an older server without the classes API answers `404` (its JSON one for unknown API
    /// paths): not supported, not an error
    #[tokio::test]
    async fn test_older_server() {
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(serde_json::json!({"detail": "Not found."})),
            )
            .mount(&server)
            .await;
        assert_eq!(client(&server).search_classes("Foo").await.unwrap(), None);
        assert_eq!(
            auth_client(&server)
                .check_classes("foo", vec![1, 2, 3])
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn test_check_classes_refused() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/classes/check/"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(serde_json::json!({"detail": "Invalid token."})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/classes/"))
            .respond_with(ResponseTemplate::new(500).set_body_string("<html>oops</html>"))
            .mount(&server)
            .await;

        // the login hint of the publish calls
        let err = auth_client(&server)
            .check_classes("foo", vec![1])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("MAGNET_TOKEN"), "{err}");
        assert!(err.contains("Invalid token."), "{err}");

        let err = client(&server)
            .search_classes("Foo")
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "The server refused the request (500 Internal Server Error)"
        );
    }
}
