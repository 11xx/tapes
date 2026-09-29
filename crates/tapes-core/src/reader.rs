//! The build of the reader that produced a projection. A reader repair can
//! change what the same source projects to under the same schema version, so
//! a consumer that keeps projections needs to tell a changed source from a
//! changed reader.

use serde::{Deserialize, Serialize};

/// The package and the source it was built from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaderIdentity {
    pub package: String,
    pub version: String,
    pub build: ReaderBuild,
}

/// Where the reader's source came from. `modified` says the reader's sources
/// differed from `revision` when it was built.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "kebab-case")]
pub enum ReaderBuild {
    Checkout { revision: String, modified: bool },
    Unknown,
}

/// This build's identity.
pub fn identity() -> ReaderIdentity {
    let revision = env!("TAPES_SOURCE_REVISION");
    ReaderIdentity {
        package: env!("CARGO_PKG_NAME").to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        build: if revision.is_empty() {
            ReaderBuild::Unknown
        } else {
            ReaderBuild::Checkout {
                revision: revision.to_owned(),
                modified: env!("TAPES_SOURCE_MODIFIED") == "true",
            }
        },
    }
}

/// The identity as one line: `agent-tapes-core 2026.9.19 (git 1a2b3c4d5e6f, modified)`.
pub fn describe() -> String {
    let identity = identity();
    let build = match &identity.build {
        ReaderBuild::Checkout { revision, modified } => format!(
            "git {}{}",
            &revision[..revision.len().min(12)],
            if *modified { ", modified" } else { "" }
        ),
        ReaderBuild::Unknown => "source revision unknown".to_owned(),
    };
    format!("{} {} ({build})", identity.package, identity.version)
}
