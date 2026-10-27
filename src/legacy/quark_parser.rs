//! A best guess at the dependencies of a legacy `.quark` file without running sclang
//! (DESIGN.md "Backwards compatibility / Legacy Quarks"). Only the value of the
//! `dependencies` key is read, and only if it is a plain array literal; anything else (e.g.
//! code like `[] ++ if(…)`) is reported, so the caller can warn and assume no dependencies.
//! [parse_fields] reads the other keys the same way, e.g. for `magnet import-legacy-quark`.

use std::collections::BTreeMap;

use url::Url;

/// A dependency of a `.quark` file, in one of the forms `Quark.parseDependency` accepts:
/// `"name"`, `"name@refspec"`, `"https://…/name"`, `"name" -> version` or
/// `[name, version, url]`. Versions and svn URLs are ignored, like SuperCollider does.
#[derive(Debug, Clone, PartialEq)]
pub struct QuarkDependency {
    pub name: String,
    /// the repository, if the dependency was given as URL
    pub url: Option<Url>,
    pub refspec: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// `"string"`
    Str(String),
    /// `\symbol` or `'symbol'`
    Symbol(String),
    /// identifiers, keywords and numbers
    Word(String),
    Punct(char),
}

fn tokenize(content: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = content.chars().collect();
    let mut tokens = vec![];
    let mut i = 0;
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '.';
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c.is_whitespace() {
            i += 1;
        } else if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if c == '"' || c == '\'' {
            let mut text = String::new();
            i += 1;
            while i < chars.len() && chars[i] != c {
                if chars[i] == '\\' {
                    i += 1;
                }
                if let Some(&escaped) = chars.get(i) {
                    text.push(escaped);
                }
                i += 1;
            }
            if i >= chars.len() {
                return Err("a string is not closed".to_string());
            }
            i += 1;
            tokens.push(match c {
                '"' => Token::Str(text),
                _ => Token::Symbol(text),
            });
        } else if c == '\\' && next.is_some_and(is_word) {
            let start = i + 1;
            i = start;
            while i < chars.len() && is_word(chars[i]) {
                i += 1;
            }
            tokens.push(Token::Symbol(chars[start..i].iter().collect()));
        } else if is_word(c) {
            let start = i;
            while i < chars.len() && is_word(chars[i]) {
                i += 1;
            }
            tokens.push(Token::Word(chars[start..i].iter().collect()));
        } else {
            tokens.push(Token::Punct(c));
            i += 1;
        }
    }
    Ok(tokens)
}

/// A literal within the `dependencies` array
enum Value {
    Text(String),
    Number,
    Array(Vec<Value>),
    Association(Box<Value>),
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position).cloned();
        self.position += 1;
        token
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(&Token::Punct(c)) {
            self.position += 1;
            return true;
        }
        false
    }

    /// `value` or `value -> value`
    fn value(&mut self) -> Result<Value, String> {
        let value = self.primary()?;
        if self.peek() == Some(&Token::Punct('-'))
            && self.tokens.get(self.position + 1) == Some(&Token::Punct('>'))
        {
            self.position += 2;
            self.primary()?;
            return Ok(Value::Association(Box::new(value)));
        }
        Ok(value)
    }

    fn primary(&mut self) -> Result<Value, String> {
        match self.next() {
            Some(Token::Str(text)) | Some(Token::Symbol(text)) => Ok(Value::Text(text)),
            Some(Token::Word(word)) if word.starts_with(|c: char| c.is_ascii_digit()) => {
                Ok(Value::Number)
            }
            Some(Token::Punct('(')) => {
                let value = self.value()?;
                match self.eat(')') {
                    true => Ok(value),
                    false => Err("a `(` is not closed".to_string()),
                }
            }
            Some(Token::Punct('#')) if self.eat('[') => self.array(),
            Some(Token::Punct('[')) => self.array(),
            other => Err(format!("unexpected {other:?}")),
        }
    }

    /// the items of an array after its `[`
    fn array(&mut self) -> Result<Value, String> {
        let mut items = vec![];
        loop {
            if self.eat(']') {
                return Ok(Value::Array(items));
            }
            items.push(self.value()?);
            if !self.eat(',') && self.peek() != Some(&Token::Punct(']')) {
                return Err("an array is not closed".to_string());
            }
        }
    }
}

/// The dependencies of a `.quark` file, an empty list if it names none, or why they can't be
/// read without running sclang
pub fn parse_dependencies(content: &str) -> Result<Vec<QuarkDependency>, String> {
    let tokens = tokenize(content)?;
    // `dependencies:` or `\dependencies:`, outside of strings and comments
    let key = tokens.windows(2).position(|pair| {
        matches!(&pair[0], Token::Word(key) | Token::Symbol(key) if key == "dependencies")
            && pair[1] == Token::Punct(':')
    });
    let Some(key) = key else {
        return Ok(vec![]);
    };
    let mut parser = Parser {
        tokens,
        position: key + 2,
    };
    let value = parser.value()?;
    // e.g. `[] ++ (…).if([…])` is code, not a literal
    if !matches!(parser.peek(), None | Some(Token::Punct(',' | ')'))) {
        return Err("the dependencies are computed by code".to_string());
    }
    let Value::Array(items) = value else {
        return Err("the dependencies are not an array".to_string());
    };
    items.into_iter().map(dependency).collect()
}

/// The top-level keys of a `.quark` file, with their value if it is a single string, symbol
/// or number (e.g. `version: 1.0`), `None` for anything else (e.g. code or arrays). The first
/// occurrence of a key wins, like for [parse_dependencies].
pub fn parse_fields(content: &str) -> Result<BTreeMap<String, Option<String>>, String> {
    let tokens = tokenize(content)?;
    let mut fields = BTreeMap::new();
    // the keys of the event `(…)` are at depth 1
    let mut depth = 0;
    for (i, token) in tokens.iter().enumerate() {
        match token {
            Token::Punct('(' | '[' | '{') => depth += 1,
            Token::Punct(')' | ']' | '}') => depth -= 1,
            Token::Word(key) | Token::Symbol(key)
                if depth == 1 && tokens.get(i + 1) == Some(&Token::Punct(':')) =>
            {
                let value = match tokens.get(i + 2..i + 4) {
                    Some(
                        [Token::Str(value) | Token::Symbol(value) | Token::Word(value), Token::Punct(',' | ')')],
                    ) => Some(value.clone()),
                    _ => None,
                };
                fields.entry(key.clone()).or_insert(value);
            }
            _ => {}
        }
    }
    Ok(fields)
}

fn dependency(value: Value) -> Result<QuarkDependency, String> {
    match value {
        Value::Text(text) => from_text(&text),
        Value::Association(key) => dependency(*key),
        // [name, version, svn url], the url is ignored like in `Quark.parseDependency`
        Value::Array(items) => match items.into_iter().next() {
            Some(Value::Text(name)) => from_text(&name),
            _ => Err("a dependency array doesn't start with a name".to_string()),
        },
        Value::Number => Err("a dependency is a number".to_string()),
    }
}

/// `name`, `name@refspec`, `url` or `url@refspec`, see `Quark.parseQuarkName`
fn from_text(text: &str) -> Result<QuarkDependency, String> {
    let (name, refspec) = match text.split_once('@') {
        Some((name, refspec)) => (name, Some(refspec.to_string())),
        None => (text, None),
    };
    if name.contains("://") {
        let url: Url = name
            .parse()
            .map_err(|_| format!("{name} is not a valid URL"))?;
        let last = url
            .path_segments()
            .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
            .ok_or_else(|| format!("{name} has no repository name"))?;
        return Ok(QuarkDependency {
            name: last.strip_suffix(".git").unwrap_or(last).to_string(),
            url: Some(url),
            refspec,
        });
    }
    if name.contains('/') {
        return Err(format!("{name} is a local path"));
    }
    Ok(QuarkDependency {
        name: name.to_string(),
        url: None,
        refspec,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/quarks")
            .join(name);
        std::fs::read_to_string(path).unwrap()
    }

    fn names(content: &str) -> Vec<String> {
        parse_dependencies(content)
            .unwrap()
            .into_iter()
            .map(|dependency| dependency.name)
            .collect()
    }

    #[test]
    fn test_real_quark_files() {
        // symbols as keys, and commented-out examples of the other forms
        assert_eq!(names(&fixture("3Dj.quark")), ["MathLib", "redUniverse"]);
        // an association within parentheses
        assert_eq!(names(&fixture("Ctk.quark")), ["XML"]);
        assert_eq!(names(&fixture("KtlLoop.quark")), ["JITLibExtensions"]);
        assert_eq!(
            names(&fixture("HyperDisCo.quark")),
            ["adclib", "JITLibExtensions", "Utopia"]
        );
        assert!(names(&fixture("Singleton.quark")).is_empty());
        let collapse = parse_dependencies(&fixture("Collapse.quark")).unwrap_err();
        assert!(collapse.contains("code"), "{collapse}");
    }

    #[test]
    fn test_dependency_forms() {
        let dependencies = parse_dependencies(
            r#"(
                name: "x", // dependencies: ["commented"]
                /* dependencies: ["commented"] */
                dependencies: [
                    "plain",
                    'symbol',
                    "pinned@tags/1.0",
                    "https://github.com/user/fromUrl.git@4b825dc",
                    ["array", 1.0, "https://svn.weirdserver.be/trunk"],
                    "assoc" -> 1.0,
                ],
                since: "2013"
            )"#,
        )
        .unwrap();
        let summary: Vec<(String, Option<String>, Option<String>)> = dependencies
            .into_iter()
            .map(|dependency| {
                (
                    dependency.name,
                    dependency.url.map(|url| url.to_string()),
                    dependency.refspec,
                )
            })
            .collect();
        let entry = |name: &str, url: Option<&str>, refspec: Option<&str>| {
            (
                name.to_string(),
                url.map(String::from),
                refspec.map(String::from),
            )
        };
        assert_eq!(
            summary,
            [
                entry("plain", None, None),
                entry("symbol", None, None),
                entry("pinned", None, Some("tags/1.0")),
                entry(
                    "fromUrl",
                    Some("https://github.com/user/fromUrl.git"),
                    Some("4b825dc")
                ),
                entry("array", None, None),
                entry("assoc", None, None),
            ]
        );
    }

    #[test]
    fn test_parse_fields() {
        let field = |content: &str, key: &str| parse_fields(content).unwrap().remove(key);

        let collapse = fixture("Collapse.quark");
        assert_eq!(
            field(&collapse, "author"),
            Some(Some("Hanns Holger Rutz (contact@sciss.de)".to_string()))
        );
        assert_eq!(field(&collapse, "version"), Some(Some("1.0.1".to_string())));
        assert_eq!(
            field(&collapse, "isCompatible"),
            Some(Some("true".to_string()))
        );
        // code is no literal
        assert_eq!(field(&collapse, "dependencies"), Some(None));

        // symbols as keys, comments in between
        let three_dj = fixture("3Dj.quark");
        assert_eq!(field(&three_dj, "name"), Some(Some("3Dj".to_string())));
        assert_eq!(
            field(&three_dj, "url"),
            Some(Some(
                "https://github.com/andresperezlopez/rt-spatialization".to_string()
            ))
        );
        assert_eq!(field(&three_dj, "version"), None);

        let fields = parse_fields(
            r#"(
                name: "x", // version: "commented"
                version: 1.0,
                summary: "a" ++ "b",
                isCompatible: { |q| q.notNil },
                nested: (version: "2.0"),
                name: "second"
            )"#,
        )
        .unwrap();
        assert_eq!(fields["name"].as_deref(), Some("x"));
        assert_eq!(fields["version"].as_deref(), Some("1.0"));
        assert_eq!(fields["summary"], None);
        assert_eq!(fields["isCompatible"], None);
        assert_eq!(fields["nested"], None);
        assert_eq!(fields.len(), 5);

        assert!(parse_fields("(name: \"x)").is_err());
    }

    #[test]
    fn test_no_or_unreadable_dependencies() {
        assert!(names("(name: \"x\", version: 1.0)").is_empty());
        // ddwCommon
        assert!(parse_dependencies(
            "(dependencies: if('Crucial'.asClass.isNil) { #[\"crucial-library\"] })"
        )
        .is_err());
        assert!(parse_dependencies("(dependencies: [\"a\"").is_err());
        assert!(parse_dependencies("(dependencies: [\"../local\"])").is_err());
        assert!(parse_dependencies("(dependencies: \"a\")").is_err());
    }
}
