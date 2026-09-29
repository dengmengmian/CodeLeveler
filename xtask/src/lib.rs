mod clean;
mod cli;
mod error;
mod exec;
mod fmt;
mod git;
mod publish;
mod qualify;
mod root;
mod scope;
mod version;

#[cfg(test)]
mod tests;

pub use cli::dispatch;
pub use error::Fail;
