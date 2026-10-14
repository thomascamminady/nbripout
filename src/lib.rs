//! nbripout: strip output from Jupyter and Zeppelin notebooks.
//!
//! A drop-in replacement for [nbstripout](https://github.com/kynan/nbstripout)
//! that produces byte-identical results while never corrupting the input.

pub mod filter_process;
pub mod fsutil;
pub mod git;
pub mod json;
pub mod notebook;
pub mod py;
pub mod strip;

pub use notebook::{Error, Mode};

/// A stripped notebook, ready to be rendered.
#[derive(Debug)]
pub struct Stripped {
    /// Whether stripping changed the notebook's content (formatting differences
    /// alone do not count, just as in nbstripout).
    pub changed: bool,
    document: notebook::Document,
}

impl Stripped {
    /// Serialize the stripped notebook. With `crlf`, structural newlines are
    /// written as `\r\n` (newlines inside strings are always escaped).
    pub fn render(self, crlf: bool) -> Vec<u8> {
        let out = self.document.render();
        if !crlf {
            return out;
        }
        let mut converted = Vec::with_capacity(out.len() + out.len() / 16);
        for &b in &out {
            if b == b'\n' {
                converted.push(b'\r');
            }
            converted.push(b);
        }
        converted
    }
}

/// Parse, normalize and strip a notebook.
pub fn strip_notebook(
    input: &[u8],
    mode: Mode,
    options: &strip::Options,
) -> Result<Stripped, Error> {
    let mut document = notebook::read(input, mode)?;
    let changed = strip::strip(&mut document, options)?;
    Ok(Stripped { changed, document })
}
