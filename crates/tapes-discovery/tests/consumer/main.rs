use agent_tapes_discovery::{Discovery, Harness, NativeSession, NativeStore, ResolveError};
use std::env;
use std::ffi::OsString;
use std::path::PathBuf;

enum DeliveryProjection {
    Codex { session_id: String },
    UnsupportedDelivery { harness: Harness },
}

fn delivery_supported(harness: Harness) -> bool {
    harness == Harness::Codex
}

fn project_delivery(session: &NativeSession) -> DeliveryProjection {
    if delivery_supported(session.harness()) {
        DeliveryProjection::Codex {
            session_id: session.id().to_owned(),
        }
    } else {
        DeliveryProjection::UnsupportedDelivery {
            harness: session.harness(),
        }
    }
}

fn store(specification: &str) -> Result<NativeStore, String> {
    let (kind, path) = specification
        .split_once('=')
        .ok_or_else(|| "store specification must be KIND=PATH".to_owned())?;
    let path = PathBuf::from(path);
    match kind {
        "claude" => Ok(NativeStore::claude(path)),
        "codex" => Ok(NativeStore::codex(path)),
        "pi" => Ok(NativeStore::pi(path)),
        "opencode-stable" => Ok(NativeStore::opencode_stable(OsString::from(path))),
        "opencode-v2" => Ok(NativeStore::opencode_v2(OsString::from(path))),
        _ => Err("unsupported native store kind".to_owned()),
    }
}

fn error_class(error: &ResolveError) -> &'static str {
    match error {
        ResolveError::InvalidQuery => "invalid-query",
        ResolveError::NotFound { .. } => "not-found",
        ResolveError::Incomplete { .. } => "incomplete",
        ResolveError::Ambiguous { .. } => "ambiguous",
        ResolveError::BackendFailed { .. } => "backend-failed",
        ResolveError::StoreFailed { .. } => "store-failed",
    }
}

fn run(arguments: &[String]) -> Result<(), String> {
    let (discovery, query) = if arguments
        .first()
        .is_some_and(|argument| argument == "--from-env")
    {
        let query = arguments
            .get(1)
            .ok_or_else(|| "environment query is missing".to_owned())?;
        (Discovery::from_env(), query.as_str())
    } else {
        if arguments.len() < 2 {
            return Err("provide a query and at least one native store".to_owned());
        }
        let query = arguments.first().expect("query was checked");
        let stores = arguments[1..]
            .iter()
            .map(|specification| store(specification))
            .collect::<Result<Vec<_>, _>>()?;
        (Discovery::new(stores), query.as_str())
    };

    match discovery.resolve(query) {
        Ok(session) => match project_delivery(&session) {
            DeliveryProjection::Codex { session_id } => println!(
                "status=target id={} harness={} store={} delivery=codex",
                session_id,
                session.harness().as_str(),
                session.store_coordinate()
            ),
            DeliveryProjection::UnsupportedDelivery { harness } => println!(
                "status=unsupported-delivery id={} harness={} store={} delivery=none",
                session.id(),
                harness.as_str(),
                session.store_coordinate()
            ),
        },
        Err(error) => println!("status=refused error={} delivery=none", error_class(&error)),
    }
    Ok(())
}

fn main() {
    if let Err(error) = run(&env::args().skip(1).collect::<Vec<_>>()) {
        eprintln!("consumer configuration error: {error}");
        std::process::exit(2);
    }
}

#[cfg(test)]
mod tests {
    use super::delivery_supported;
    use agent_tapes_discovery::Harness;

    #[test]
    fn codex_is_the_only_supported_delivery_target() {
        assert!(delivery_supported(Harness::Codex));
        assert!(!delivery_supported(Harness::Claude));
        assert!(!delivery_supported(Harness::OpenCode));
        assert!(!delivery_supported(Harness::Pi));
    }
}
