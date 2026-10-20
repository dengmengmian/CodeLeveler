//! Direct, session-owned terminal operations. Input writes must never be replayed.
use crate::SessionId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum TerminalOperation {
    Create { cols: u16, rows: u16 },
    Write { data: Vec<u8> },
    Resize { cols: u16, rows: u16 },
    Observe { cursor: u64, max_bytes: usize },
    Close,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalRequest {
    pub session_id: SessionId,
    pub terminal_id: String,
    pub operation: TerminalOperation,
}
impl TerminalRequest {
    pub fn validate(&self) -> Result<(), String> {
        let id = self.terminal_id.as_bytes();
        if id.len() != 36
            || !id.iter().enumerate().all(|(i, b)| {
                if [8, 13, 18, 23].contains(&i) {
                    *b == b'-'
                } else {
                    b.is_ascii_hexdigit() && !b.is_ascii_uppercase()
                }
            })
        {
            return Err("terminal_id must be a lowercase UUID".into());
        }
        match &self.operation {
            TerminalOperation::Create { cols, rows } | TerminalOperation::Resize { cols, rows }
                if !(1..=1000).contains(cols) || !(1..=1000).contains(rows) =>
            {
                Err("terminal dimensions must be 1..1000".into())
            }
            TerminalOperation::Write { data } if data.is_empty() || data.len() > 65536 => {
                Err("terminal input must contain 1..65536 bytes".into())
            }
            TerminalOperation::Observe { max_bytes, .. }
                if *max_bytes == 0 || *max_bytes > 65536 =>
            {
                Err("terminal observation must contain 1..65536 bytes".into())
            }
            _ => Ok(()),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalState {
    pub terminal_id: String,
    pub shell: String,
    pub cwd: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub pid: Option<u32>,
    pub cols: u16,
    pub rows: u16,
    pub output_start: u64,
    pub output_end: u64,
    pub next_cursor: u64,
    pub data: Vec<u8>,
    pub truncated: bool,
}
#[cfg(test)]
mod tests {
    use super::*;
    fn request(operation: TerminalOperation) -> TerminalRequest {
        TerminalRequest {
            session_id: SessionId::new("s"),
            terminal_id: "12345678-1234-1234-1234-123456789abc".into(),
            operation,
        }
    }
    #[test]
    fn terminal_requests_reject_invalid_identity_and_unbounded_payloads() {
        let mut valid = request(TerminalOperation::Create { cols: 80, rows: 24 });
        assert!(valid.validate().is_ok());
        valid.terminal_id = "../../other".into();
        assert!(valid.validate().is_err());
        assert!(
            request(TerminalOperation::Resize { cols: 0, rows: 24 })
                .validate()
                .is_err()
        );
        assert!(
            request(TerminalOperation::Write {
                data: vec![0; 65537]
            })
            .validate()
            .is_err()
        );
        assert!(
            request(TerminalOperation::Observe {
                cursor: 0,
                max_bytes: 65536
            })
            .validate()
            .is_ok()
        );
        assert!(
            request(TerminalOperation::Observe {
                cursor: 0,
                max_bytes: 65537
            })
            .validate()
            .is_err()
        );
    }
}
