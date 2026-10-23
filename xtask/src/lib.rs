mod arch;
mod clean;
mod cli;
mod clock;
mod error;
mod exec;
mod fmt;
mod git;
mod github;
mod hash;
mod json;
mod lock;
mod pipeline;
mod qualify;
mod report;
mod root;
mod scope;
mod state;
mod version;

#[cfg(test)]
mod pipeline_tests;
#[cfg(test)]
mod tests;

pub use cli::dispatch;
pub use error::Fail;
