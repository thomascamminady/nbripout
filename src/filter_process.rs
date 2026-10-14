//! git's long-running filter process protocol (`filter.<driver>.process`).
//!
//! With a plain `clean` filter git spawns one process per notebook. With
//! `process`, a single nbripout instance serves every file of a git command over
//! pkt-line framed stdin/stdout. See gitattributes(5), "Long Running Filter Process".

use std::io::{self, BufReader, BufWriter, Read, Write};

/// Largest payload of a single pkt-line (65520 bytes minus the 4 byte header).
const MAX_PACKET_DATA: usize = 65516;

#[derive(Debug, PartialEq, Eq)]
enum Packet {
    Flush,
    Data(Vec<u8>),
}

fn protocol_error(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Read one pkt-line. `Ok(None)` signals a clean end of input between packets.
fn read_packet(r: &mut impl Read) -> io::Result<Option<Packet>> {
    let mut header = [0u8; 4];
    let mut filled = 0;
    while filled < header.len() {
        match r.read(&mut header[filled..])? {
            0 if filled == 0 => return Ok(None),
            0 => return Err(protocol_error("truncated pkt-line header")),
            n => filled += n,
        }
    }
    let len = std::str::from_utf8(&header)
        .ok()
        .and_then(|h| usize::from_str_radix(h, 16).ok())
        .ok_or_else(|| protocol_error("invalid pkt-line header"))?;
    match len {
        0 => Ok(Some(Packet::Flush)),
        1..=4 => Err(protocol_error(format!("unexpected pkt-line length {len}"))),
        _ => {
            let mut data = vec![0u8; len - 4];
            r.read_exact(&mut data)?;
            Ok(Some(Packet::Data(data)))
        }
    }
}

/// Read text packets up to the next flush, with trailing newlines removed.
/// `Ok(None)` if the input ended before the first packet.
fn read_text_list(r: &mut impl Read) -> io::Result<Option<Vec<String>>> {
    let mut lines = Vec::new();
    loop {
        match read_packet(r)? {
            None if lines.is_empty() => return Ok(None),
            None => return Err(protocol_error("unexpected end of input")),
            Some(Packet::Flush) => return Ok(Some(lines)),
            Some(Packet::Data(data)) => {
                let text = String::from_utf8_lossy(&data);
                lines.push(text.strip_suffix('\n').unwrap_or(&text).to_owned());
            }
        }
    }
}

fn read_content(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut content = Vec::new();
    loop {
        match read_packet(r)? {
            None => return Err(protocol_error("unexpected end of input in content")),
            Some(Packet::Flush) => return Ok(content),
            Some(Packet::Data(data)) => content.extend_from_slice(&data),
        }
    }
}

fn write_data(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    for chunk in data.chunks(MAX_PACKET_DATA) {
        write!(w, "{:04x}", chunk.len() + 4)?;
        w.write_all(chunk)?;
    }
    Ok(())
}

fn write_text(w: &mut impl Write, line: &str) -> io::Result<()> {
    write_data(w, format!("{line}\n").as_bytes())
}

fn write_flush(w: &mut impl Write) -> io::Result<()> {
    w.write_all(b"0000")
}

/// Run the filter process until git closes the pipe. `clean` is called with the
/// pathname and content of every file git wants cleaned; smudging is the identity.
pub fn serve<R, W, F>(input: R, output: W, mut clean: F) -> io::Result<()>
where
    R: Read,
    W: Write,
    F: FnMut(&str, &[u8]) -> Result<Vec<u8>, String>,
{
    let mut r = BufReader::with_capacity(1 << 16, input);
    let mut w = BufWriter::with_capacity(1 << 16, output);

    let welcome = read_text_list(&mut r)?.ok_or_else(|| protocol_error("no handshake"))?;
    if welcome.first().map(String::as_str) != Some("git-filter-client")
        || !welcome.iter().any(|l| l == "version=2")
    {
        return Err(protocol_error(format!(
            "unsupported handshake: {welcome:?}"
        )));
    }
    write_text(&mut w, "git-filter-server")?;
    write_text(&mut w, "version=2")?;
    write_flush(&mut w)?;
    w.flush()?;

    let offered = read_text_list(&mut r)?.ok_or_else(|| protocol_error("no capabilities"))?;
    for capability in ["capability=clean", "capability=smudge"] {
        if offered.iter().any(|c| c == capability) {
            write_text(&mut w, capability)?;
        }
    }
    write_flush(&mut w)?;
    w.flush()?;

    while let Some(headers) = read_text_list(&mut r)? {
        let field = |name: &str| {
            headers
                .iter()
                .find_map(|h| h.strip_prefix(name)?.strip_prefix('='))
                .unwrap_or_default()
        };
        let command = field("command");
        let pathname = field("pathname");
        let content = read_content(&mut r)?;
        let result = match command {
            "clean" => clean(pathname, &content),
            "smudge" => Ok(content),
            other => Err(format!("unsupported filter command `{other}`")),
        };
        match result {
            Ok(filtered) => {
                write_text(&mut w, "status=success")?;
                write_flush(&mut w)?;
                write_data(&mut w, &filtered)?;
                write_flush(&mut w)?;
                // Empty list: keep "status=success".
                write_flush(&mut w)?;
            }
            Err(msg) => {
                eprintln!("nbripout: {pathname}: {msg}");
                write_text(&mut w, "status=error")?;
                write_flush(&mut w)?;
            }
        }
        w.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(items: &[Option<&[u8]>]) -> Vec<u8> {
        let mut out = Vec::new();
        for item in items {
            match item {
                Some(data) => write_data(&mut out, data).unwrap(),
                None => write_flush(&mut out).unwrap(),
            }
        }
        out
    }

    #[test]
    fn speaks_the_protocol() {
        let big = vec![b'x'; MAX_PACKET_DATA + 10];
        let input = packets(&[
            Some(b"git-filter-client\n"),
            Some(b"version=2\n"),
            None,
            Some(b"capability=clean\n"),
            Some(b"capability=smudge\n"),
            Some(b"capability=delay\n"),
            None,
            Some(b"command=clean\n"),
            Some(b"pathname=a.ipynb\n"),
            None,
            Some(&big),
            None,
            Some(b"command=smudge\n"),
            Some(b"pathname=b.ipynb\n"),
            None,
            Some(b"raw"),
            None,
            Some(b"command=clean\n"),
            Some(b"pathname=bad.ipynb\n"),
            None,
            None,
        ]);
        let mut output = Vec::new();
        serve(&input[..], &mut output, |path, content| {
            if path == "bad.ipynb" {
                Err("broken".into())
            } else {
                Ok(content[..3].to_vec())
            }
        })
        .unwrap();

        let mut r = &output[..];
        let mut next = || read_packet(&mut r).unwrap().unwrap();
        let text = |s: &str| Packet::Data(format!("{s}\n").into_bytes());
        let expected = [
            text("git-filter-server"),
            text("version=2"),
            Packet::Flush,
            text("capability=clean"),
            text("capability=smudge"),
            Packet::Flush,
            text("status=success"),
            Packet::Flush,
            Packet::Data(b"xxx".to_vec()),
            Packet::Flush,
            Packet::Flush,
            text("status=success"),
            Packet::Flush,
            Packet::Data(b"raw".to_vec()),
            Packet::Flush,
            Packet::Flush,
            text("status=error"),
            Packet::Flush,
        ];
        for packet in expected {
            assert_eq!(next(), packet);
        }
        assert!(read_packet(&mut r).unwrap().is_none());
    }
}
