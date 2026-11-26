//! Store-independent identifiers, settings values and diagnostics.
mod diagnostic;
mod location;
mod metadata;
pub use diagnostic::*;
pub use location::*;
pub use metadata::*;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::{path::Path, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataClass {
    Standard,
    Archived,
}

impl FromStr for DataClass {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "standard" => Ok(Self::Standard),
            "archived" => Ok(Self::Archived),
            _ => Err("data-class must be standard or archived".into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreTier {
    Bulk,
    Standard,
}

impl FromStr for RestoreTier {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "bulk" => Ok(Self::Bulk),
            "standard" => Ok(Self::Standard),
            _ => Err("restore tier must be Bulk or Standard".into()),
        }
    }
}

/// Built only from supplied lines; never reads .gitignore or Git configuration.
pub fn compile_ignore(base: &Path, patterns: &[String]) -> Result<Gitignore, String> {
    let mut builder = GitignoreBuilder::new(base);
    for pattern in patterns {
        if pattern.contains(['\n', '\r', '\0']) {
            return Err("an ignore pattern must be a single line without NUL".into());
        }
        builder.add_line(None, pattern).map_err(|e| e.to_string())?;
    }
    builder.build().map_err(|e| e.to_string())
}

/// CLI list escaping is independent of gitignore escaping: only \, and \\
/// are consumed here; \!, \# and other gitignore escapes are preserved.
pub fn parse_ignore_list(value: &str) -> Vec<String> {
    if value.is_empty() {
        return vec![];
    }
    let mut result = Vec::new();
    let mut line = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ',' => {
                result.push(std::mem::take(&mut line));
            }
            '\\' if chars.peek().is_some_and(|c| *c == ',' || *c == '\\') => {
                line.push(chars.next().unwrap());
            }
            c => line.push(c),
        }
    }
    result.push(line);
    result
}
