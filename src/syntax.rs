// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{ops::Range, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    Plain,
    Rust,
    C,
    Cpp,
    Python,
    JavaScript,
    TypeScript,
    Json,
    Toml,
    Yaml,
    Shell,
    Go,
    Java,
    Html,
    Css,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Comment,
    Keyword,
    String,
    Number,
}

impl Language {
    pub fn of(path: &Path) -> Self {
        match path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "rs" => Self::Rust,
            "c" | "h" => Self::C,
            "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => Self::Cpp,
            "py" | "pyw" => Self::Python,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "ts" | "tsx" => Self::TypeScript,
            "json" | "jsonc" => Self::Json,
            "toml" => Self::Toml,
            "yaml" | "yml" => Self::Yaml,
            "sh" | "bash" | "zsh" => Self::Shell,
            "go" => Self::Go,
            "java" => Self::Java,
            "html" | "htm" | "xml" => Self::Html,
            "css" => Self::Css,
            _ => Self::Plain,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Plain => "Plain text",
            Self::Rust => "Rust",
            Self::C => "C",
            Self::Cpp => "C++",
            Self::Python => "Python",
            Self::JavaScript => "JavaScript",
            Self::TypeScript => "TypeScript",
            Self::Json => "JSON",
            Self::Toml => "TOML",
            Self::Yaml => "YAML",
            Self::Shell => "Shell",
            Self::Go => "Go",
            Self::Java => "Java",
            Self::Html => "Markup",
            Self::Css => "CSS",
        }
    }

    fn line_comment(self) -> Option<&'static str> {
        match self {
            Self::Python | Self::Toml | Self::Yaml | Self::Shell => Some("#"),
            Self::Rust
            | Self::C
            | Self::Cpp
            | Self::JavaScript
            | Self::TypeScript
            | Self::Go
            | Self::Java => Some("//"),
            _ => None,
        }
    }

    fn block_comment(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Rust
            | Self::C
            | Self::Cpp
            | Self::JavaScript
            | Self::TypeScript
            | Self::Go
            | Self::Java
            | Self::Css => Some(("/*", "*/")),
            Self::Html => Some(("<!--", "-->")),
            _ => None,
        }
    }

    fn keyword(self, word: &str) -> bool {
        let words = match self {
            Self::Rust => {
                "as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while"
            }
            Self::Python => {
                "and as assert async await break class continue def del elif else except False finally for from global if import in is lambda None nonlocal not or pass raise return True try while with yield"
            }
            Self::JavaScript | Self::TypeScript => {
                "as async await break case catch class const continue debugger default delete do else enum export extends false finally for from function if implements import in instanceof interface let new null of private protected public return static super switch this throw true try type typeof undefined var void while yield"
            }
            Self::C | Self::Cpp => {
                "auto bool break case char class const continue default delete do double else enum extern false float for if int long namespace new nullptr private protected public return short signed sizeof static struct switch template this true typedef union unsigned using void volatile while"
            }
            Self::Java => {
                "abstract assert boolean break byte case catch char class continue default do double else enum extends false final finally float for if implements import instanceof int interface long native new null package private protected public record return short static super switch synchronized this throw throws true try void volatile while"
            }
            Self::Go => {
                "break case chan const continue default defer else fallthrough false for func go goto if import interface map nil package range return select struct switch true type var"
            }
            Self::Shell => {
                "case do done elif else esac export fi for function if in local return select then until while"
            }
            Self::Json | Self::Toml | Self::Yaml => "false null true",
            _ => "",
        };
        words
            .split_ascii_whitespace()
            .any(|keyword| keyword == word)
    }

    /// Byte ranges into the original line. Bound work even for generated or
    /// minified source. No full-document parse is run on the UI thread.
    pub fn highlight(self, line: &str, max_byte: usize) -> Vec<(Range<usize>, Style)> {
        if self == Self::Plain {
            return Vec::new();
        }
        let end = line.floor_char_boundary(line.len().min(max_byte).min(8192));
        let line = &line[..end];
        let mut tokens = Vec::new();
        let mut start = 0;
        while start < line.len() {
            let rest = &line[start..];
            let ch = rest.chars().next().unwrap();
            let (length, style) = if self
                .line_comment()
                .is_some_and(|mark| rest.starts_with(mark))
            {
                (rest.len(), Some(Style::Comment))
            } else if let Some((open, close)) = self
                .block_comment()
                .filter(|(open, _)| rest.starts_with(open))
            {
                let length = rest[open.len()..]
                    .find(close)
                    .map_or(rest.len(), |i| i + open.len() + close.len());
                (length, Some(Style::Comment))
            } else if ch == '"'
                || (ch == '\'' && self != Self::Json)
                || (ch == '`'
                    && matches!(
                        self,
                        Self::JavaScript | Self::TypeScript | Self::Go | Self::Shell
                    ))
            {
                let mut escaped = false;
                let length = rest.char_indices().skip(1).find_map(|(i, next)| {
                    let closed = next == ch && !escaped;
                    escaped = next == '\\' && !escaped;
                    closed.then_some(i + next.len_utf8())
                });
                // An unclosed apostrophe in Rust can introduce a lifetime.
                if ch == '\'' && self == Self::Rust && length.is_none() {
                    (1, None)
                } else {
                    (length.unwrap_or(rest.len()), Some(Style::String))
                }
            } else if ch.is_alphanumeric() || ch == '_' {
                let length = rest
                    .find(|c: char| {
                        !c.is_alphanumeric()
                            && c != '_'
                            && unicode_width::UnicodeWidthChar::width(c) != Some(0)
                    })
                    .unwrap_or(rest.len());
                let style = if ch.is_ascii_digit() {
                    Some(Style::Number)
                } else if self.keyword(&rest[..length]) {
                    Some(Style::Keyword)
                } else {
                    None
                };
                (length, style)
            } else {
                (ch.len_utf8(), None)
            };
            if let Some(style) = style {
                tokens.push((start..start + length, style));
            }
            start += length;
        }
        tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_boundaries_comments_and_escaped_strings() {
        let line = "let β = \"世\\\"界\"; // async";
        let tokens: Vec<_> = Language::Rust
            .highlight(line, line.len())
            .into_iter()
            .map(|(range, style)| (&line[range], style))
            .collect();
        assert_eq!(
            tokens,
            [
                ("let", Style::Keyword),
                ("\"世\\\"界\"", Style::String),
                ("// async", Style::Comment)
            ]
        );
        assert!(
            Language::Rust
                .highlight(&"界".repeat(4096), usize::MAX)
                .is_empty()
        );
    }
}
