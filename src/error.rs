use std::fmt;

use anyhow::anyhow;

#[derive(Debug)]
pub enum Error {
    Refuse(String),
    Stopped(String),
    Failed(String),
    Unexpected(anyhow::Error),
}

impl Error {
    pub fn refuse(message: impl Into<String>) -> Self {
        Error::Refuse(message.into())
    }

    pub fn stopped(message: impl Into<String>) -> Self {
        Error::Stopped(message.into())
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Error::Failed(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Refuse(message) | Error::Stopped(message) | Error::Failed(message) => {
                f.write_str(message)
            }
            Error::Unexpected(err) => write!(f, "{err:#}"),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Unexpected(anyhow!(err))
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::Unexpected(anyhow!(err))
    }
}

impl From<reqwest::Error> for Error {
    /// Drops the request URL: LFS hrefs are presigned and base URLs may carry
    /// credentials, and neither may reach stderr.
    fn from(err: reqwest::Error) -> Self {
        Error::Unexpected(anyhow!(err.without_url()))
    }
}

impl From<anyhow::Error> for Error {
    fn from(err: anyhow::Error) -> Self {
        Error::Unexpected(err)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Exit {
    pub code: i32,
    pub message: String,
}

impl From<Error> for Exit {
    fn from(err: Error) -> Self {
        match err {
            Error::Refuse(message) => Exit { code: 2, message },
            Error::Stopped(message) => Exit { code: 3, message },
            Error::Failed(message) => Exit { code: 4, message },
            Error::Unexpected(err) => Exit {
                code: 1,
                message: format!("{err:#}"),
            },
        }
    }
}
