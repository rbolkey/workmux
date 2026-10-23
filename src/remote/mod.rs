//! Remotes: project hosts other than the workstation, reached through a
//! transport and spoken to over a control channel (ADR 0001).

pub mod channel;
pub mod handshake;
pub mod transport;
pub mod connection;
pub mod serve;
pub mod channels;
#[cfg(test)]
pub mod testing;
