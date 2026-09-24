use tapes_discovery::{
    resolve_with_sources, CandidatePage, IdentityRecord, IdentitySource, ResolveError, Resolved,
};

#[derive(Clone, Debug, PartialEq)]
struct Row {
    id: String,
    harness: String,
    store: String,
}

impl Row {
    fn new(id: &str, harness: &str, store: &str) -> Self {
        Self {
            id: id.to_owned(),
            harness: harness.to_owned(),
            store: store.to_owned(),
        }
    }
}

impl IdentityRecord for Row {
    fn id(&self) -> &str {
        &self.id
    }

    fn harness(&self) -> &str {
        &self.harness
    }

    fn store_coordinate(&self) -> Option<&str> {
        Some(&self.store)
    }
}

struct Source {
    harness: String,
    exact: Result<Option<Row>, String>,
    page: CandidatePage<Row, String>,
    shared: bool,
    terminal: bool,
    available: bool,
    lists: std::cell::Cell<usize>,
}

impl Source {
    fn empty(harness: &str) -> Self {
        Self {
            harness: harness.to_owned(),
            exact: Ok(None),
            page: page(Vec::new(), true),
            shared: false,
            terminal: false,
            available: true,
            lists: std::cell::Cell::new(0),
        }
    }

    fn with_exact(mut self, exact: Result<Option<Row>, String>) -> Self {
        self.exact = exact;
        self
    }

    fn with_page(mut self, page: CandidatePage<Row, String>) -> Self {
        self.page = page;
        self
    }

    fn shared(mut self) -> Self {
        self.shared = true;
        self
    }

    fn terminal(mut self) -> Self {
        self.terminal = true;
        self
    }

    fn unavailable(mut self) -> Self {
        self.available = false;
        self
    }
}

impl IdentitySource<Row> for Source {
    fn harness(&self) -> &str {
        &self.harness
    }

    fn locate_exact(&self, _query: &str) -> Result<Option<Row>, String> {
        self.exact.clone()
    }

    fn candidates(&self, _limit: usize) -> CandidatePage<Row, String> {
        self.lists.set(self.lists.get() + 1);
        self.page.clone()
    }

    fn shares_session_ids(&self) -> bool {
        self.shared
    }

    fn terminal_exact_failure(&self) -> bool {
        self.terminal
    }

    fn available(&self) -> bool {
        self.available
    }
}

fn page(records: Vec<Row>, complete: bool) -> CandidatePage<Row, String> {
    CandidatePage {
        records,
        scanned: 0,
        visited_entries: 0,
        complete,
        failures: Vec::new(),
        unreadable_ids: Vec::new(),
    }
}

fn resolve(
    sources: &[&dyn IdentitySource<Row>],
    query: &str,
) -> Result<Resolved<Row>, ResolveError> {
    resolve_with_sources(sources, query)
}

#[test]
fn exact_lookup_skips_candidate_enumeration_and_beats_unrelated_failure() {
    let hit = Source::empty("codex").with_exact(Ok(Some(Row::new("native-id", "codex", "store"))));
    let broken = Source::empty("pi").with_exact(Err("unrelated read failed".to_owned()));
    let resolved = resolve(&[&hit, &broken], "native-id").unwrap();
    assert_eq!(resolved.record.id(), "native-id");
    assert_eq!(hit.lists.get(), 0);
    assert_eq!(broken.lists.get(), 0);
}

#[test]
fn distinct_exact_sources_are_ambiguous() {
    let claude = Source::empty("claude").with_exact(Ok(Some(Row::new("same", "claude", "a"))));
    let codex = Source::empty("codex").with_exact(Ok(Some(Row::new("same", "codex", "b"))));
    let error = resolve(&[&claude, &codex], "same").unwrap_err();
    assert!(matches!(error, ResolveError::Ambiguous { .. }));
}

#[test]
fn shared_open_code_stores_prefer_the_earlier_hit_and_refuse_after_its_failure() {
    let stable = Source::empty("opencode")
        .shared()
        .with_exact(Ok(Some(Row::new("ses_same", "opencode", "stable"))));
    let v2 = Source::empty("opencode")
        .shared()
        .with_exact(Ok(Some(Row::new("ses_same", "opencode", "v2"))));
    let selected = resolve(&[&stable, &v2], "ses_same").unwrap();
    assert_eq!(selected.record.store, "stable");

    let failed_stable = Source::empty("opencode")
        .shared()
        .with_exact(Err("stable query failed".to_owned()));
    let error = resolve(&[&failed_stable, &v2], "ses_same").unwrap_err();
    assert!(matches!(error, ResolveError::StoreFailed { .. }));
}

#[test]
fn unreadable_higher_priority_open_code_row_reserves_its_identity() {
    let stable = Source::empty("opencode").shared().with_page(CandidatePage {
        records: Vec::new(),
        scanned: 1,
        visited_entries: 0,
        complete: false,
        failures: vec!["stable row is malformed".to_owned()],
        unreadable_ids: vec!["ses_shared".to_owned()],
    });
    let v2 = Source::empty("opencode")
        .shared()
        .with_page(page(vec![Row::new("ses_shared", "opencode", "v2")], true));
    assert!(matches!(
        resolve(&[&stable, &v2], "ses_"),
        Err(ResolveError::StoreFailed { .. })
    ));
}

#[test]
fn a_complete_unique_prefix_resolves_to_the_canonical_id() {
    let source = Source::empty("claude").with_page(page(
        vec![Row::new("prefix-canonical-id", "claude", "store")],
        true,
    ));
    let resolved = resolve(&[&source], "prefix-").unwrap();
    assert_eq!(resolved.record.id(), "prefix-canonical-id");
}

#[test]
fn failed_unavailable_source_makes_a_single_prefix_candidate_incomplete() {
    let unreadable = Source::empty("claude")
        .with_exact(Err("store inspection failed".to_owned()))
        .unavailable();
    let candidate = Source::empty("codex").with_page(page(
        vec![Row::new("prefix-one", "codex", "codex-store")],
        true,
    ));
    let error = resolve(&[&unreadable, &candidate], "prefix-").unwrap_err();
    let ResolveError::Incomplete { diagnostics, .. } = error else {
        panic!("expected incomplete coverage, got {error:?}");
    };
    assert!(diagnostics
        .iter()
        .any(|diagnostic| diagnostic.contains("store inspection failed")));
}

#[test]
fn an_incomplete_single_prefix_candidate_is_not_a_resolution() {
    let source =
        Source::empty("pi").with_page(page(vec![Row::new("prefix-first", "pi", "store")], false));
    assert!(matches!(
        resolve(&[&source], "prefix-"),
        Err(ResolveError::Incomplete { .. })
    ));
}

#[test]
fn two_observed_prefix_candidates_are_ambiguous_even_when_incomplete() {
    let source = Source::empty("codex").with_page(page(
        vec![
            Row::new("prefix-first", "codex", "store"),
            Row::new("prefix-second", "codex", "store"),
        ],
        false,
    ));
    assert!(matches!(
        resolve(&[&source], "prefix-"),
        Err(ResolveError::Ambiguous { .. })
    ));
}

#[test]
fn a_known_exact_row_wins_over_an_incomplete_candidate_page() {
    let source = Source::empty("codex").with_page(page(
        vec![Row::new("full-canonical-id", "codex", "store")],
        false,
    ));
    let resolved = resolve(&[&source], "full-canonical-id").unwrap();
    assert_eq!(resolved.record.id(), "full-canonical-id");
}

#[test]
fn terminal_supplied_source_failure_does_not_fall_through_to_prefixes() {
    let input = Source::empty("input")
        .terminal()
        .with_exact(Err("ambiguous occurrence".to_owned()))
        .with_page(page(vec![Row::new("native-id", "input", "supplied")], true));
    assert!(matches!(
        resolve(&[&input], "native-id"),
        Err(ResolveError::BackendFailed { .. })
    ));
    assert_eq!(input.lists.get(), 0);
}

#[test]
fn missing_source_and_invalid_queries_keep_their_error_classes() {
    let missing = Source {
        available: false,
        ..Source::empty("claude")
    };
    assert!(matches!(
        resolve(&[&missing], "absent"),
        Err(ResolveError::NotFound { .. })
    ));
    assert!(matches!(
        resolve(&[&missing], "  "),
        Err(ResolveError::InvalidQuery)
    ));
}
