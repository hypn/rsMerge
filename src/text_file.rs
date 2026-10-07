//! Loading and saving text files, preserving encoding, BOM and line endings.

use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252};
use std::path::{Path, PathBuf};

/// How many leading bytes are checked for NULs when deciding a file is binary.
const BINARY_SNIFF_LEN: usize = 8000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Eol {
    Lf,
    CrLf,
}

impl Eol {
    pub fn as_str(self) -> &'static str {
        match self {
            Eol::Lf => "\n",
            Eol::CrLf => "\r\n",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Eol::Lf => "LF",
            Eol::CrLf => "CRLF",
        }
    }
}

pub struct TextFile {
    pub path: PathBuf,
    /// Lines without their line endings. An empty file has no lines.
    pub lines: Vec<String>,
    pub encoding: &'static Encoding,
    pub bom: bool,
    pub eol: Eol,
    pub final_newline: bool,
    /// False for a side that doesn't exist yet (e.g. a file only in the other folder);
    /// saving creates it.
    pub exists: bool,
}

pub enum Loaded {
    Text(TextFile),
    Binary,
}

pub fn load(path: &Path) -> std::io::Result<Loaded> {
    let bytes = std::fs::read(path)?;
    let (encoding, bom_len) = match Encoding::for_bom(&bytes) {
        Some((encoding, len)) => (encoding, len),
        None => {
            if bytes[..bytes.len().min(BINARY_SNIFF_LEN)].contains(&0) {
                return Ok(Loaded::Binary);
            }
            let encoding = if std::str::from_utf8(&bytes).is_ok() { UTF_8 } else { WINDOWS_1252 };
            (encoding, 0)
        }
    };
    let (text, _) = encoding.decode_without_bom_handling(&bytes[bom_len..]);
    let (lines, eol, final_newline) = split_lines(&text);
    Ok(Loaded::Text(TextFile {
        path: path.to_path_buf(),
        lines,
        encoding,
        bom: bom_len > 0,
        eol,
        final_newline,
        exists: true,
    }))
}

/// Loads a file, or returns an empty placeholder if it doesn't exist.
pub fn load_or_missing(path: &Path) -> std::io::Result<Loaded> {
    if path.exists() { load(path) } else { Ok(Loaded::Text(TextFile::missing(path))) }
}

fn split_lines(text: &str) -> (Vec<String>, Eol, bool) {
    if text.is_empty() {
        return (Vec::new(), Eol::Lf, false);
    }
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    let eol = if crlf > lf { Eol::CrLf } else { Eol::Lf };
    let final_newline = text.ends_with('\n');
    let mut lines: Vec<String> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect();
    if final_newline {
        lines.pop();
    }
    (lines, eol, final_newline)
}

impl TextFile {
    pub fn missing(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            lines: Vec::new(),
            encoding: UTF_8,
            bom: false,
            eol: if cfg!(windows) { Eol::CrLf } else { Eol::Lf },
            final_newline: true,
            exists: false,
        }
    }

    pub fn encoding_name(&self) -> String {
        let name = self.encoding.name();
        if self.bom { format!("{name} BOM") } else { name.to_string() }
    }

    /// Writes the lines back using the file's original encoding, BOM and line ending style.
    pub fn save(&mut self) -> Result<(), String> {
        let eol = self.eol.as_str();
        let mut text = self.lines.join(eol);
        if self.final_newline && !self.lines.is_empty() {
            text.push_str(eol);
        }
        let mut bytes = Vec::with_capacity(text.len() + 3);
        if self.encoding == UTF_16LE || self.encoding == UTF_16BE {
            let le = self.encoding == UTF_16LE;
            if self.bom {
                bytes.extend_from_slice(if le { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] });
            }
            for unit in text.encode_utf16() {
                bytes.extend_from_slice(&if le { unit.to_le_bytes() } else { unit.to_be_bytes() });
            }
        } else {
            if self.bom && self.encoding == UTF_8 {
                bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
            }
            let (encoded, _, unmappable) = self.encoding.encode(&text);
            if unmappable {
                return Err(format!(
                    "{} contains characters that can't be saved as {}",
                    self.path.display(),
                    self.encoding.name()
                ));
            }
            bytes.extend_from_slice(&encoded);
        }
        if !self.exists
            && let Some(parent) = self.path.parent()
        {
            std::fs::create_dir_all(parent).map_err(|e| format!("Couldn't create {}: {e}", parent.display()))?;
        }
        std::fs::write(&self.path, bytes).map_err(|e| format!("Couldn't save {}: {e}", self.path.display()))?;
        self.exists = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_and_detects_line_endings() {
        assert_eq!(split_lines(""), (vec![], Eol::Lf, false));
        assert_eq!(split_lines("a\nb\n"), (vec!["a".into(), "b".into()], Eol::Lf, true));
        assert_eq!(split_lines("a\r\nb"), (vec!["a".into(), "b".into()], Eol::CrLf, false));
        assert_eq!(split_lines("\n"), (vec!["".into()], Eol::Lf, true));
    }
}
