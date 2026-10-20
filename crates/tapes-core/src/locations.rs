use chrono::{DateTime, Utc};

use crate::model::{
    LocationCoverage, LocationObservation, SessionLocations, MAX_LOCATION_OBSERVATIONS,
};

/// The longest field value an observation keeps; a longer one is not
/// evidence of a place and is dropped as if the record had not named it.
const MAX_FIELD_BYTES: usize = 4096;

/// What one record states about where the session is working.
#[derive(Default)]
pub(crate) struct Place<'a> {
    pub at: Option<DateTime<Utc>>,
    pub cwd: &'a str,
    pub branch: Option<&'a str>,
    pub head_commit: Option<&'a str>,
    pub repository_url: Option<&'a str>,
}

/// Folds records, in recording order, into the distinct places they show.
#[derive(Default)]
pub(crate) struct LocationStream {
    kept: std::collections::VecDeque<LocationObservation>,
    omitted: usize,
    /// The newest place observed, whether or not the cap still keeps it.
    current: Option<LocationObservation>,
}

impl LocationStream {
    /// A record that states every field it records: a change in any of them
    /// is a new place.
    pub fn observe(&mut self, place: Place<'_>) {
        let Some(next) = observation(place) else {
            return;
        };
        if self
            .current
            .as_ref()
            .is_none_or(|current| !same_place(current, &next))
        {
            self.keep(next);
        }
    }

    /// A record that states only a directory: a different one is a new place
    /// that carries nothing but the directory, since the record says nothing
    /// of the branch or commit there.
    pub fn observe_directory(&mut self, at: Option<DateTime<Utc>>, cwd: &str) {
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.cwd == cwd)
        {
            return;
        }
        self.observe(Place {
            at,
            cwd,
            ..Place::default()
        });
    }

    fn keep(&mut self, next: LocationObservation) {
        if self.kept.len() == MAX_LOCATION_OBSERVATIONS {
            self.kept.pop_front();
            self.omitted += 1;
        }
        self.kept.push_back(next.clone());
        self.current = Some(next);
    }

    pub fn finish(self, whole: bool) -> SessionLocations {
        SessionLocations {
            coverage: if whole {
                LocationCoverage::Whole
            } else {
                LocationCoverage::FromWindowStart
            },
            observations: self.kept.into(),
            omitted: self.omitted,
        }
    }
}

fn same_place(current: &LocationObservation, next: &LocationObservation) -> bool {
    current.cwd == next.cwd
        && current.branch == next.branch
        && current.head_commit == next.head_commit
        && current.repository_url == next.repository_url
}

fn observation(place: Place<'_>) -> Option<LocationObservation> {
    fn bounded(value: &str) -> Option<&str> {
        (!value.is_empty() && value.len() <= MAX_FIELD_BYTES).then_some(value)
    }
    let cwd = bounded(place.cwd)?;
    Some(LocationObservation {
        at: place.at,
        cwd: cwd.to_owned(),
        branch: place.branch.and_then(bounded).map(str::to_owned),
        head_commit: place.head_commit.and_then(bounded).map(str::to_owned),
        repository_url: place
            .repository_url
            .and_then(bounded)
            .map(redact_credentials),
    })
}

/// The URL without the `user:password@` a `scheme://` URL may carry, which is
/// a credential and no part of where the repository is. An scp-style address
/// (`git@host:path`) keeps its user, which names no secret.
fn redact_credentials(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://{host}{tail}"),
        None => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_leave_a_scheme_url_and_not_an_scp_address() {
        assert_eq!(
            redact_credentials("https://token:x@example.test/org/repo.git"),
            "https://example.test/org/repo.git"
        );
        assert_eq!(
            redact_credentials("ssh://git@example.test/org/repo.git"),
            "ssh://example.test/org/repo.git"
        );
        assert_eq!(
            redact_credentials("git@example.test:org/repo.git"),
            "git@example.test:org/repo.git"
        );
        assert_eq!(redact_credentials("/srv/repos/x"), "/srv/repos/x");
        assert_eq!(
            redact_credentials("https://example.test/a@b"),
            "https://example.test/a@b"
        );
    }

    #[test]
    fn the_cap_keeps_the_newest_places_and_counts_the_rest() {
        let mut stream = LocationStream::default();
        for index in 0..MAX_LOCATION_OBSERVATIONS + 3 {
            stream.observe_directory(None, &format!("/work/{index}"));
        }
        let locations = stream.finish(true);
        assert_eq!(locations.observations.len(), MAX_LOCATION_OBSERVATIONS);
        assert_eq!(locations.omitted, 3);
        assert_eq!(locations.observations[0].cwd, "/work/3");
    }
}
