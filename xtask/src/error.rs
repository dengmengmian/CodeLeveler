use std::fmt;

/// A failed `./dev` command. `code` is the stable reason an agent can match.
#[derive(Debug)]
pub struct Fail {
    pub code: String,
    pub detail: String,
    pub exit: i32,
}

impl Fail {
    pub fn new(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            detail: detail.into(),
            exit: 1,
        }
    }

    pub fn exit(mut self, exit: i32) -> Self {
        self.exit = exit;
        self
    }
}

impl fmt::Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FAIL {}\n{}", self.code, self.detail)
    }
}

/// A finished command. Non-zero `code` is a gate result, not an infrastructure failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub code: i32,
    pub text: String,
}

impl Report {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            code: 0,
            text: text.into(),
        }
    }

    pub fn with_code(code: i32, text: impl Into<String>) -> Self {
        Self {
            code,
            text: text.into(),
        }
    }
}
