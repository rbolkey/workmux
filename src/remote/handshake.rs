//! The sentinel hello that opens a control channel.
//!
//! Remote workmux prints a newline and then the sentinel hello line before
//! speaking the protocol. The workstation discards everything before that
//! line, so shell-startup output on the remote cannot corrupt the stream.
//! The leading newline ends any partial noise line, so the sentinel always
//! starts a line of its own.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

/// Version of the control channel protocol. Bump on incompatible changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Marks the hello line; everything before it is discarded.
pub const SENTINEL: &str = "WORKMUX-CONTROL-CHANNEL";

/// How much noise the workstation discards before giving up on a hello.
const MAX_NOISE_BYTES: usize = 1024 * 1024;

/// Optional features this binary supports when serving a control channel.
/// Requests a remote does not announce are not sent to it.
const CAPABILITIES: &[&str] = &[];

/// What remote workmux announces when a control channel opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// Remote workmux release.
    pub workmux: String,
    pub protocol: u32,
    /// Optional features this remote workmux supports.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl Hello {
    /// The hello this binary announces.
    pub fn current() -> Self {
        Self {
            workmux: env!("CARGO_PKG_VERSION").to_string(),
            protocol: PROTOCOL_VERSION,
            capabilities: CAPABILITIES.iter().map(|c| c.to_string()).collect(),
        }
    }

    /// Refuse a protocol mismatch. A different workmux release with the same
    /// protocol is allowed; the returned warning says so.
    pub fn check(&self, remote: &str) -> Result<Option<String>> {
        let ours = env!("CARGO_PKG_VERSION");
        if self.protocol != PROTOCOL_VERSION {
            return Err(anyhow!(
                "remote {remote} runs workmux {} (control protocol {}), but this workstation \
                 speaks control protocol {PROTOCOL_VERSION}: install workmux {ours} on {remote}",
                self.workmux,
                self.protocol,
            ));
        }
        if self.workmux != ours {
            return Ok(Some(format!(
                "remote {remote} runs workmux {}, this workstation runs {ours}",
                self.workmux
            )));
        }
        Ok(None)
    }
}

/// Write the leading newline and the sentinel hello line.
pub fn write_hello(writer: &mut impl Write, hello: &Hello) -> Result<()> {
    let line = format!("\n{SENTINEL} {}\n", serde_json::to_string(hello)?);
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    Ok(())
}

/// Read up to and including the sentinel hello line, discarding noise.
pub fn read_hello(reader: &mut impl BufRead) -> Result<Hello> {
    let prefix = format!("{SENTINEL} ");
    let mut discarded = 0;
    while discarded <= MAX_NOISE_BYTES {
        let Some(line) = super::channel::read_line(reader)? else {
            return Err(anyhow!("remote workmux ended before its hello"));
        };
        if let Some(json) = line.strip_prefix(prefix.as_bytes()) {
            return serde_json::from_slice(json)
                .map_err(|error| anyhow!("remote workmux sent an unreadable hello: {error}"));
        }
        discarded += line.len() + 1;
    }
    Err(anyhow!(
        "remote workmux printed {MAX_NOISE_BYTES} bytes without a hello"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn hello(workmux: &str, protocol: u32) -> Hello {
        Hello {
            workmux: workmux.to_string(),
            protocol,
            capabilities: vec![],
        }
    }

    fn hello_bytes(hello: &Hello) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_hello(&mut bytes, hello).unwrap();
        bytes
    }

    #[test]
    fn the_current_hello_names_this_release_and_protocol() {
        let current = Hello::current();

        assert_eq!(current.workmux, env!("CARGO_PKG_VERSION"));
        assert_eq!(current.protocol, 1);
    }

    #[test]
    fn noise_before_the_sentinel_is_discarded() {
        let sent = hello("1.2.3", 1);
        let mut stream = b"Welcome to devbox\r\n\x1b[32mrc says hi\x1b[0m\npartial".to_vec();
        stream.extend(hello_bytes(&sent));
        stream.extend(b"{\"id\":1}\n");
        let mut reader = Cursor::new(stream);

        let received = read_hello(&mut reader).unwrap();

        assert_eq!(received, sent);
        // The protocol stream starts right after the hello line.
        let mut rest = String::new();
        reader.read_line(&mut rest).unwrap();
        assert_eq!(rest, "{\"id\":1}\n");
    }

    #[test]
    fn a_hello_line_ending_in_crlf_is_accepted() {
        let line = format!(
            "\r\n{SENTINEL} {}\r\n",
            serde_json::to_string(&hello("1.2.3", 1)).unwrap()
        );

        let received = read_hello(&mut Cursor::new(line.into_bytes())).unwrap();

        assert_eq!(received.workmux, "1.2.3");
    }

    #[test]
    fn a_stream_that_ends_before_the_hello_is_an_error() {
        let error = read_hello(&mut Cursor::new(
            b"bash: workmux: command not found\n".to_vec(),
        ))
        .unwrap_err();

        assert!(
            error.to_string().contains("ended before its hello"),
            "{error}"
        );
    }

    #[test]
    fn a_protocol_mismatch_is_refused_naming_the_version_to_install() {
        let error = hello("0.0.1", PROTOCOL_VERSION + 1)
            .check("devbox")
            .unwrap_err()
            .to_string();

        assert!(error.contains("devbox"), "{error}");
        assert!(
            error.contains(&format!("protocol {}", PROTOCOL_VERSION + 1)),
            "{error}"
        );
        assert!(
            error.contains(&format!("install workmux {}", env!("CARGO_PKG_VERSION"))),
            "{error}"
        );
    }

    #[test]
    fn a_different_release_with_the_same_protocol_only_warns() {
        let warning = hello("0.0.1", PROTOCOL_VERSION)
            .check("devbox")
            .unwrap()
            .expect("a warning");

        assert!(warning.contains("devbox runs workmux 0.0.1"), "{warning}");
        assert!(warning.contains(env!("CARGO_PKG_VERSION")), "{warning}");
        assert_eq!(Hello::current().check("devbox").unwrap(), None);
    }
}
