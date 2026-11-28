//! The proof handed from the Navidrome exchange to the session decision (§7.2).
use super::kept::{Fingerprint, Purpose, Scope, Try};
use super::proto;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Kind {
    Pc,
    Phone,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Description {
    pub name: String,
    pub kind: Kind,
    pub version: proto::Version,
}

/// §7.2. A verified receipt is used within five seconds of getUser's answer.
pub const RECEIPT_MS: u64 = 5_000;

/// Only this module's successful end of a spent try can construct a receipt. S2 can
/// inspect it and give it once to kept::Turn::finish. It holds no bearer or token.
#[derive(Debug)]
pub struct Receipt {
    ticket: Try,
    target: Scope,
    device_hash: Fingerprint,
    description: Description,
    spent: u64,
    made: u64,
}

impl Receipt {
    pub(in crate::nexus) fn ticket_ref(&self) -> &Try {
        &self.ticket
    }
    pub fn source(&self) -> &Scope {
        self.ticket.scope()
    }
    pub fn target(&self) -> &Scope {
        &self.target
    }
    pub fn account(&self) -> &str {
        &self.target.account
    }
    pub fn purpose(&self) -> &Purpose {
        self.ticket.purpose()
    }
    pub fn description(&self) -> &Description {
        &self.description
    }
    pub fn spent(&self) -> u64 {
        self.spent
    }
    pub fn made(&self) -> u64 {
        self.made
    }
    pub(in crate::nexus) fn into_parts(self) -> (Try, Scope, Fingerprint, Description) {
        (self.ticket, self.target, self.device_hash, self.description)
    }
}

#[cfg(test)]
mod receipt_tests;
