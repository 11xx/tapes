use agent_tapes_core::{
    backend::{codex::CodexBackend, Backend},
    search, ListFilters, ListSort,
};
use std::path::PathBuf;

fn backends() -> Vec<Box<dyn Backend>> {
    vec![Box::new(CodexBackend::new(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex"),
    ))]
}

#[test]
fn whole_search_caps_coordinates_and_reads_after_selection() {
    let stores = backends();
    let filters = ListFilters {
        search: Some(""),
        ..Default::default()
    };
    let result = search::list(
        &stores,
        None,
        None,
        &filters,
        search::Options {
            limit: 1,
            sort: ListSort::Oldest,
            match_limit: 1,
        },
    )
    .unwrap();
    let report = result.full_search.as_ref().unwrap();
    assert!(report.selection_truncated);
    assert_eq!(report.sessions_read, 1);
    assert_eq!(result.sessions.len(), 1);
    let coverage = &report.coverage[0];
    assert_eq!(coverage.matches.len(), 1);
    assert!(coverage.matches_truncated);
    assert_eq!(coverage.matches[0].ordinal, 0);
    assert!(coverage.matches[0].record_ref.is_some());
    assert!(coverage.matches[0].ts.is_some());
    let wire = serde_json::to_string(&result).unwrap();
    let decoded: agent_tapes_core::SessionList = serde_json::from_str(&wire).unwrap();
    assert_eq!(decoded.full_search.unwrap().sessions_read, 1);
}

#[test]
fn nonmatches_keep_coverage_and_metadata_filters_precede_reads() {
    let stores = backends();
    let filters = ListFilters {
        search: Some("impossible fixture phrase"),
        ..Default::default()
    };
    let result = search::list(
        &stores,
        None,
        None,
        &filters,
        search::Options {
            limit: 2,
            sort: ListSort::Newest,
            match_limit: 10,
        },
    )
    .unwrap();
    assert!(result.sessions.is_empty());
    assert_eq!(result.full_search.unwrap().coverage.len(), 2);
    let filters = ListFilters {
        model: Some("impossible model"),
        ..filters
    };
    let result = search::list(
        &stores,
        None,
        None,
        &filters,
        search::Options {
            limit: 2,
            sort: ListSort::Newest,
            match_limit: 10,
        },
    )
    .unwrap();
    let report = result.full_search.unwrap();
    assert_eq!(report.sessions_read, 0);
    assert!(!report.selection_truncated);
}

#[test]
fn whole_search_finds_a_turn_before_the_bounded_search_window() {
    let stores: Vec<Box<dyn Backend>> = vec![Box::new(CodexBackend::new(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/whole-search/codex"),
    ))];
    let filters = ListFilters {
        search: Some("EARLY WHOLE-SEARCH NEEDLE"),
        ..Default::default()
    };
    let bounded = agent_tapes_core::list_with_backends_options(
        &stores,
        None,
        None,
        1,
        &filters,
        ListSort::Newest,
    )
    .unwrap();
    assert!(bounded.sessions.is_empty());
    let result = search::list(
        &stores,
        None,
        None,
        &filters,
        search::Options {
            limit: 1,
            sort: ListSort::Newest,
            match_limit: 10,
        },
    )
    .unwrap();
    assert_eq!(result.sessions.len(), 1);
    let report = result.full_search.unwrap();
    assert!(!report.selection_truncated);
    assert!(report.coverage[0].whole_recording_read);
    assert_eq!(report.coverage[0].matches[0].ordinal, 0);
    let filters = ListFilters {
        since: Some(agent_tapes_core::parse_activity_timestamp("2026-01-02").unwrap()),
        ..filters
    };
    let result = search::list(
        &stores,
        None,
        None,
        &filters,
        search::Options {
            limit: 1,
            sort: ListSort::Newest,
            match_limit: 10,
        },
    )
    .unwrap();
    assert_eq!(result.full_search.unwrap().sessions_read, 0);
}

struct BoundedOnly(CodexBackend);
impl Backend for BoundedOnly {
    fn harness(&self) -> &'static str {
        "codex"
    }
    fn available(&self) -> bool {
        true
    }
    fn list(
        &self,
        query: &agent_tapes_core::backend::Query,
    ) -> anyhow::Result<agent_tapes_core::backend::Listing> {
        self.0.list(query)
    }
    fn locate(&self, id: &str) -> anyhow::Result<Option<agent_tapes_core::model::Session>> {
        self.0.locate(id)
    }
    fn transcript(
        &self,
        _: &agent_tapes_core::model::Session,
        _: usize,
    ) -> anyhow::Result<agent_tapes_core::model::Transcript> {
        panic!("whole search must not fall back to a bounded reader")
    }
    fn kinds(&self) -> agent_tapes_core::model::KindDeclaration {
        self.0.kinds()
    }
}

#[test]
fn unsupported_store_is_reported_once_without_bounded_fallback() {
    let stores: Vec<Box<dyn Backend>> = vec![Box::new(BoundedOnly(CodexBackend::new(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex"),
    )))];
    let filters = ListFilters {
        search: Some(""),
        ..Default::default()
    };
    let result = search::list(
        &stores,
        None,
        None,
        &filters,
        search::Options {
            limit: 2,
            sort: ListSort::Newest,
            match_limit: 1,
        },
    )
    .unwrap();
    assert_eq!(result.unsearched.len(), 1);
    assert!(result.sessions.is_empty());
    let report = result.full_search.unwrap();
    assert_eq!(report.sessions_read, 0);
    assert_eq!(report.coverage.len(), 2);
    assert!(report
        .coverage
        .iter()
        .all(|session| !session.whole_recording_read));
}

#[test]
fn session_read_limit_is_global_across_harnesses() {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let stores: Vec<Box<dyn Backend>> = vec![
        Box::new(CodexBackend::new(fixtures.join("whole-search/codex"))),
        Box::new(agent_tapes_core::backend::pi::PiBackend::new(
            fixtures.join("rewound/pi"),
        )),
    ];
    let filters = ListFilters {
        search: Some(""),
        ..Default::default()
    };
    let result = search::list(
        &stores,
        None,
        None,
        &filters,
        search::Options {
            limit: 1,
            sort: ListSort::Newest,
            match_limit: 0,
        },
    )
    .unwrap();
    let report = result.full_search.unwrap();
    assert_eq!(report.sessions_read, 1);
    assert_eq!(report.coverage.len(), 1);
    assert_eq!(report.coverage[0].harness, "pi");
    assert_eq!(report.coverage[0].session, "session-rewound");
    assert!(report.coverage[0].matches.is_empty());
    assert!(
        report.coverage[0].matches_truncated,
        "{:?}",
        report.coverage[0]
    );
    assert!(report.selection_truncated);
}
