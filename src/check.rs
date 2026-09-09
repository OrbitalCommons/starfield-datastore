use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::Arc,
};

pub type Predicate = dyn Fn(&[u8]) -> std::result::Result<(), String> + Send + Sync;

#[derive(Clone)]
pub enum ContentCheck {
    Sha256(String),
    Magic {
        prefixes: Vec<Vec<u8>>,
        trim_leading_whitespace: bool,
    },
    MinBytes(u64),
    NotHtml,
    Custom(Arc<Predicate>),
    All(Vec<ContentCheck>),
    None,
}

impl std::fmt::Debug for ContentCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sha256(s) => f.debug_tuple("Sha256").field(s).finish(),
            Self::Magic {
                prefixes,
                trim_leading_whitespace,
            } => f
                .debug_struct("Magic")
                .field("prefixes", prefixes)
                .field("trim_leading_whitespace", trim_leading_whitespace)
                .finish(),
            Self::MinBytes(n) => f.debug_tuple("MinBytes").field(n).finish(),
            Self::NotHtml => f.write_str("NotHtml"),
            Self::Custom(_) => f.write_str("Custom(<predicate>)"),
            Self::All(checks) => f.debug_tuple("All").field(checks).finish(),
            Self::None => f.write_str("None"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CheckFailure {
    pub check: String,
    pub got: String,
}

impl ContentCheck {
    pub fn magic(prefixes: Vec<Vec<u8>>, trim_leading_whitespace: bool) -> Self {
        Self::Magic {
            prefixes,
            trim_leading_whitespace,
        }
    }
    pub fn custom(f: Arc<Predicate>) -> Self {
        Self::Custom(f)
    }
    pub fn default_binary() -> Self {
        Self::All(vec![Self::NotHtml, Self::MinBytes(1024)])
    }

    pub fn check(&self, bytes: &[u8]) -> std::result::Result<(), CheckFailure> {
        self.check_reader(&mut std::io::Cursor::new(bytes), bytes.len() as u64)
    }

    /// Built-in checks stream files; only a custom predicate loads the full body.
    pub fn check_file(&self, path: &Path) -> std::result::Result<(), CheckFailure> {
        if let Self::Sha256(expected) = self {
            let actual = digest_file(path).map_err(io_failure)?;
            return if &actual == expected {
                Ok(())
            } else {
                Err(CheckFailure {
                    check: "Sha256".into(),
                    got: format!("digest {actual}"),
                })
            };
        }
        let mut file = File::open(path).map_err(io_failure)?;
        let len = file.metadata().map_err(io_failure)?.len();
        self.check_reader(&mut file, len)
    }

    fn check_reader(
        &self,
        reader: &mut (impl Read + Seek),
        len: u64,
    ) -> std::result::Result<(), CheckFailure> {
        reader.seek(SeekFrom::Start(0)).map_err(io_failure)?;
        let fail = |name: &str, got: String| {
            Err(CheckFailure {
                check: name.into(),
                got,
            })
        };
        match self {
            Self::None => Ok(()),
            Self::All(checks) => {
                for check in checks {
                    check.check_reader(reader, len)?;
                }
                Ok(())
            }
            Self::MinBytes(min) if len < *min => {
                fail("MinBytes", format!("{len} bytes, expected at least {min}"))
            }
            Self::MinBytes(_) => Ok(()),
            Self::Sha256(expected) => {
                let mut hasher = Sha256::new();
                std::io::copy(reader, &mut hasher).map_err(io_failure)?;
                let actual = format!("{:x}", hasher.finalize());
                if &actual == expected {
                    Ok(())
                } else {
                    fail("Sha256", format!("digest {actual}"))
                }
            }
            Self::Magic {
                prefixes,
                trim_leading_whitespace,
            } => {
                let max = prefixes.iter().map(Vec::len).max().unwrap_or(0);
                if prefixes.is_empty() || prefixes.iter().any(Vec::is_empty) {
                    return fail("Magic", "no nonempty signature configured".into());
                }
                let mut buffered = std::io::BufReader::new(reader);
                let mut first = [0];
                let mut head = Vec::with_capacity(max);
                while buffered.read(&mut first).map_err(io_failure)? != 0 {
                    if !*trim_leading_whitespace || !first[0].is_ascii_whitespace() {
                        head.push(first[0]);
                        break;
                    }
                }
                buffered
                    .take(max.saturating_sub(head.len()) as u64)
                    .read_to_end(&mut head)
                    .map_err(io_failure)?;
                if prefixes.iter().any(|p| head.starts_with(p)) {
                    Ok(())
                } else {
                    fail("Magic", format!("{len} bytes with an unexpected signature"))
                }
            }
            Self::NotHtml => {
                let mut head = Vec::new();
                reader
                    .take(8192)
                    .read_to_end(&mut head)
                    .map_err(io_failure)?;
                let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                let head = head.trim_start_matches('\u{feff}').trim_start();
                if head.starts_with("<!doctype html")
                    || head.starts_with("<html")
                    || head.starts_with("<head")
                    || head.starts_with("<body")
                    || head.contains("<form")
                    || head.contains("<html")
                {
                    fail(
                        "NotHtml",
                        "HTML document or login form (body redacted)".into(),
                    )
                } else {
                    Ok(())
                }
            }
            Self::Custom(f) => {
                let mut body = Vec::new();
                reader.read_to_end(&mut body).map_err(io_failure)?;
                f(&body).map_err(|got| CheckFailure {
                    check: "Custom".into(),
                    got,
                })
            }
        }
    }
}

fn io_failure(error: std::io::Error) -> CheckFailure {
    CheckFailure {
        check: "Read".into(),
        got: error.to_string(),
    }
}

pub(crate) fn digest_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_rejects_html_and_short_bodies() {
        assert!(ContentCheck::default_binary().check(&vec![0; 1024]).is_ok());
        assert!(ContentCheck::default_binary().check(b"small").is_err());
        assert!(ContentCheck::default_binary()
            .check(format!(" \n<html><form>{}", "x".repeat(2000)).as_bytes())
            .is_err());
    }
    #[test]
    fn magic_trim_is_explicit_and_custom_captures() {
        assert!(ContentCheck::magic(vec![b"ABC".to_vec()], true)
            .check(b"\n ABC")
            .is_ok());
        assert!(ContentCheck::magic(vec![b"ABC".to_vec()], false)
            .check(b" ABC")
            .is_err());
        let prefix = vec![1, 2];
        let check = ContentCheck::custom(Arc::new(move |bytes| {
            if bytes.starts_with(&prefix) {
                Ok(())
            } else {
                Err("wrong prefix".into())
            }
        }));
        assert!(check.check(&[1, 2, 3]).is_ok());
        assert_eq!(format!("{check:?}"), "Custom(<predicate>)");
    }
}
