use std::fmt::Display;

/// An error with hints on how to fix it, which `magnet` prints after the error and its causes
#[derive(Debug)]
pub struct HintedError {
    pub message: String,
    pub hints: Vec<String>,
}

impl Display for HintedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for HintedError {}
