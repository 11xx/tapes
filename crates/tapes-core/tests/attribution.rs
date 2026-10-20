//! A session's consumption divided among the work its evidence names:
//! segments, estimate windows, layers, selection, and the arithmetic their
//! figures are added up with.

use std::borrow::Cow;
use std::collections::BTreeSet;

use agent_tapes_core::attribution::{
    even_amount, even_split, sum_amounts, sum_counts, Allocation, Attribution, Basis, Conservation,
    Event, Evidence, Figure, Figures, Owner, Part, Piece, Plan, Reason, Requests, Segment, Span,
    Take, Tier, Window,
};
use agent_tapes_core::span::{Instant, PlacedRequest, RequestTimeline};

fn at(clock: &str) -> Instant {
    let clock = if clock.len() == 5 {
        format!("{clock}:00")
    } else {
        clock.to_string()
    };
    agent_tapes_core::span::parse_instant(&format!("2026-01-01T{clock}Z")).unwrap()
}

fn nanosecond() -> chrono::Duration {
    chrono::Duration::nanoseconds(1)
}

fn key(name: &str) -> String {
    format!("change:{name}")
}

fn span(work: &str, from: &str, to: Option<&str>) -> Span<String> {
    Span {
        work: key(work),
        run: None,
        from: at(from),
        to: to.map(at),
        tier: Tier::Bound,
    }
}

fn run(work: &str, from: &str, to: &str) -> Span<String> {
    Span {
        run: Some(format!("run-{work}")),
        ..span(work, from, Some(to))
    }
}

fn event(work: &str, clock: &str) -> Event<String> {
    Event {
        at: at(clock),
        work: key(work),
        tier: Tier::Matched,
    }
}

fn selected(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| key(name)).collect()
}

/// A figure with whole token counters and an amount of money.
#[derive(Clone, Debug, PartialEq)]
struct Tokens {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    usd: Option<f64>,
}

impl Tokens {
    fn input(input: u64) -> Self {
        Self {
            input: Some(input),
            output: Some(input % 7),
            cache_read: Some(0),
            usd: None,
        }
    }

    fn sum<'a>(parts: impl IntoIterator<Item = (&'a Tokens, u64)> + Clone) -> Tokens {
        let pick = |field: fn(&Tokens) -> Option<u64>| {
            sum_counts(parts.clone().into_iter().map(|(part, n)| (field(part), n)))
        };
        Tokens {
            input: pick(|tokens| tokens.input),
            output: pick(|tokens| tokens.output),
            cache_read: pick(|tokens| tokens.cache_read),
            usd: sum_amounts(parts.clone().into_iter().map(|(part, n)| (part.usd, n))),
        }
    }
}

impl Figure for Tokens {
    fn even_share(&self, shares: usize, index: usize) -> Self {
        let split = |count: Option<u64>| count.map(|count| even_split(count, shares, index));
        Tokens {
            input: split(self.input),
            output: split(self.output),
            cache_read: split(self.cache_read),
            usd: self.usd.map(|amount| even_amount(amount, shares)),
        }
    }
}

/// A recording's counted requests: each one's time, where it has one, and
/// what it consumed.
struct Recording {
    requests: Vec<(Option<Instant>, Tokens)>,
}

impl Recording {
    fn of(requests: &[(&str, u64)]) -> Self {
        Self {
            requests: requests
                .iter()
                .map(|(clock, input)| (Some(at(clock)), Tokens::input(*input)))
                .collect(),
        }
    }

    fn timeline(&self) -> RequestTimeline {
        RequestTimeline {
            requests: self
                .requests
                .iter()
                .enumerate()
                .map(|(index, (timestamp, _))| PlacedRequest {
                    ordinal: index + 1,
                    timestamp: *timestamp,
                    id: None,
                })
                .collect(),
        }
    }

    fn whole(&self) -> Tokens {
        Tokens::sum(self.requests.iter().map(|(_, tokens)| (tokens, 1)))
    }

    /// The figure of the requests timed inside `[from, to)`, and how many.
    fn within(&self, from: Option<Instant>, to: Option<Instant>) -> (Tokens, u64) {
        let held: Vec<&Tokens> = self
            .requests
            .iter()
            .filter(|(timestamp, _)| {
                timestamp.is_some_and(|at| {
                    from.is_none_or(|from| at >= from) && to.is_none_or(|to| at < to)
                })
            })
            .map(|(_, tokens)| tokens)
            .collect();
        (
            Tokens::sum(held.iter().map(|tokens| (*tokens, 1))),
            held.len() as u64,
        )
    }

    /// One figure per segment and window of `plan`, and the untimed
    /// requests', read independently of the plan's own placement.
    fn read<K>(&self, plan: &Plan<K>) -> (Requests, Figures<Tokens>) {
        let segments: Vec<(Tokens, u64)> = plan
            .segments
            .iter()
            .map(|segment| self.within(segment.from, segment.to))
            .collect();
        let windows: Vec<(Tokens, u64)> = plan
            .windows
            .iter()
            .map(|window| self.within(window.from, window.to))
            .collect();
        let untimed: Vec<&Tokens> = self
            .requests
            .iter()
            .filter(|(timestamp, _)| timestamp.is_none())
            .map(|(_, tokens)| tokens)
            .collect();
        let requests = Requests {
            segments: segments.iter().map(|(_, n)| *n).collect(),
            windows: windows.iter().map(|(_, n)| *n).collect(),
            remainder: untimed.len() as u64,
        };
        let figures = Figures {
            segments: segments.into_iter().map(|(figure, _)| figure).collect(),
            windows: windows.into_iter().map(|(figure, _)| figure).collect(),
            remainder: Some(Tokens::sum(untimed.iter().map(|tokens| (*tokens, 1)))),
        };
        (requests, figures)
    }
}

/// A plan read from `recording` and attributed with `method`.
fn attribute(
    recording: &Recording,
    plan: &Plan<String>,
    method: Option<Allocation>,
) -> (Attribution<String>, Figures<Tokens>) {
    let (requests, figures) = recording.read(plan);
    assert_eq!(plan.place(&recording.timeline()), requests);
    (plan.attribute(&requests, method, true).unwrap(), figures)
}

fn total(figures: &Figures<Tokens>, parts: &[Part]) -> Tokens {
    let held: Vec<(Cow<'_, Tokens>, u64)> = figures.parts(parts).unwrap();
    Tokens::sum(held.iter().map(|(figure, n)| (figure.as_ref(), *n)))
}

fn input(figures: &Figures<Tokens>, parts: &[Part]) -> Option<u64> {
    total(figures, parts).input
}

fn reasons(reasons: &[Reason]) -> Vec<&'static str> {
    reasons.iter().map(|reason| reason.as_str()).collect()
}

// Claim: segments cut the timeline at every span edge, each with every item
// covering all of it, and they tile the timeline from its beginning to its
// end. Rejects a segment two neighbours share, and an open span that stops
// covering at its last edge.
#[test]
fn segments_cut_the_timeline_at_every_boundary_and_never_share_a_request() {
    let plan = Plan::new(
        &[
            span("a", "10:01", Some("10:05")),
            span("b", "10:03", Some("10:08")),
            span("c", "10:09", None),
        ],
        &[],
        None,
    );
    let covered: Vec<Vec<String>> = plan
        .segments
        .iter()
        .map(|segment| {
            segment
                .work
                .iter()
                .map(|cover| cover.work.clone())
                .collect()
        })
        .collect();
    assert_eq!(
        covered,
        [
            vec![],
            vec![key("a")],
            vec![key("a"), key("b")],
            vec![key("b")],
            vec![],
            vec![key("c")],
        ]
    );
    assert_eq!(plan.segments.first().unwrap().from, None);
    assert_eq!(plan.segments.last().unwrap().to, None);
    for pair in plan.segments.windows(2) {
        assert_eq!(pair[0].to, pair[1].from);
    }
}

// Claim: a participant's events cut the segments no item covers within each
// working stretch: requests up to an event serve its item, those after a
// stretch's last event serve the same item, a stretch with no event serves
// none, and a segment an item covers is not cut. Rejects an estimate that
// crosses an idle gap, drops the requests after a stretch's last event, or
// touches measured segments.
#[test]
fn working_stretches_and_events_cut_the_uncovered_segments() {
    let events = [event("a", "10:20"), event("b", "10:50")];
    // One stretch from 10:00, and another from 10:40 with no event of its
    // own before the second verdict at 10:50.
    let plan = Plan::new(&[], &events, Some(&[at("10:00"), at("10:40")]));
    let cut: Vec<(Option<Instant>, Option<Instant>, Option<String>)> = plan
        .windows
        .iter()
        .map(|window| {
            (
                window.from,
                window.to,
                window.work.as_ref().map(|(work, _)| work.clone()),
            )
        })
        .collect();
    assert_eq!(
        cut,
        vec![
            (None, Some(at("10:20")), Some(key("a"))),
            (
                Some(at("10:20") + nanosecond()),
                Some(at("10:40")),
                Some(key("a"))
            ),
            (Some(at("10:40")), Some(at("10:50")), Some(key("b"))),
            (Some(at("10:50") + nanosecond()), None, Some(key("b"))),
        ]
    );
    assert!(plan
        .windows
        .iter()
        .all(|window| plan.segments[window.segment].work.is_empty()));
    assert!(plan.windows.iter().all(|window| window
        .work
        .as_ref()
        .is_some_and(|(_, tier)| *tier == Tier::Matched)));
    // A stretch with no event of the participant serves no item.
    let plan = Plan::new(&[], &events, Some(&[at("10:00"), at("10:30"), at("10:45")]));
    assert!(plan
        .windows
        .iter()
        .any(|window| window.from == Some(at("10:30"))
            && window.to == Some(at("10:45"))
            && window.work.is_none()));
}

// Claim: segment figures that exceed the whole, as a counter reset leaves
// them, name the counter that does not conserve, and figures that add up
// name none. Rejects reporting conservation the figures do not show.
#[test]
fn a_split_names_the_counters_its_segments_do_not_conserve() {
    let plan = Plan::new(&[span("a", "10:00", Some("10:10"))], &[], None);
    let requests = Requests {
        segments: vec![1, 1, 0],
        windows: Vec::new(),
        remainder: 0,
    };
    let attribution = plan.attribute(&requests, None, true).unwrap();
    let figures = |first: u64, second: u64| Figures {
        segments: vec![
            Tokens::input(first),
            Tokens::input(second),
            Tokens::input(0),
        ],
        windows: Vec::new(),
        remainder: None,
    };
    let held = |figures: &Figures<Tokens>, whole: u64| {
        let parts: Vec<Part> = attribution
            .conserving
            .parts
            .iter()
            .map(|(part, _)| *part)
            .collect();
        let mut conservation = Conservation::default();
        conservation.compare("input", input(figures, &parts), Some(whole));
        conservation
    };
    let conserved = held(&figures(3, 4), 7);
    assert!(conserved.differs.is_empty());
    assert_eq!(conserved.status(), "conserved");
    let reset = held(&figures(3, 4), 5);
    assert_eq!(reset.differs, vec!["input".to_string()]);
    assert_eq!(reset.status(), "not-conserved");
    let mut unknown = Conservation::default();
    unknown.compare("output", None, Some(1));
    unknown.compare("reasoning", None, None);
    assert_eq!(unknown.uncompared, vec!["output".to_string()]);
    assert_eq!(unknown.status(), "unestablished");
}

// Claim: even shares keep tokens whole and add up to the figure: each is the
// quotient, and the first shares take one token each of the remainder.
// Rejects shares that lose or invent a token, and a remainder given to the
// last shares.
#[test]
fn even_shares_add_up_to_the_figure() {
    let figure = Tokens {
        input: Some(10),
        output: Some(7),
        cache_read: Some(2),
        usd: Some(0.3),
    };
    let shares: Vec<Tokens> = (0..3).map(|index| figure.even_share(3, index)).collect();
    let summed = Tokens::sum(shares.iter().map(|share| (share, 1)));
    assert_eq!(
        (summed.input, summed.output, summed.cache_read),
        (Some(10), Some(7), Some(2))
    );
    assert_eq!(shares[0].input, Some(4));
    assert_eq!(shares[2].input, Some(3));
    assert_eq!(shares[0].usd, Some(0.3 / 3.0));
}

// Claim: a session acting on two items one after the other is divided
// exactly: each item's share holds the requests inside its event window, and
// what lies before, between and after is unattributed with that reason. A
// view of one item leaves the other's share outside its figure as an
// unselected item. Rejects handing the between request to a neighbour, and
// an unselected share inside the view's total.
#[test]
fn sequential_items_split_exactly_and_name_what_lies_outside() {
    let recording = Recording::of(&[
        ("09:50", 1),
        ("10:05", 10),
        ("10:15", 100),
        ("10:25", 1000),
        ("10:40", 10000),
    ]);
    let events = [
        event("first", "10:00"),
        event("first", "10:10"),
        event("second", "10:20"),
        event("second", "10:30"),
    ];
    let plan = Plan::new(&[], &events, None);
    let (attribution, figures) = attribute(&recording, &plan, None);
    let layer = &attribution.conserving;
    assert_eq!(
        input(&figures, &layer.figure(&selected(&["first"]))),
        Some(10)
    );
    assert_eq!(
        input(&figures, &layer.figure(&selected(&["second"]))),
        Some(1000)
    );
    let shares = layer.shares();
    assert!(shares.iter().all(|share| share.evidence
        == Evidence {
            tier: Tier::Matched,
            basis: Basis::EventWindow
        }));
    let (unattributed, why) = attribution.unattributed().unwrap();
    assert_eq!(
        reasons(&why),
        ["after last item", "before first item", "between items"]
    );
    assert_eq!(input(&figures, &unattributed), Some(10101));
    assert_eq!(
        input(&figures, &layer.figure(&selected(&["first", "second"]))),
        Some(1010)
    );

    // A view of one item: the other's share is outside, and named so.
    let view = layer.select(&selected(&["first"]));
    assert_eq!(input(&figures, &view.figure), Some(10));
    assert_eq!(view.unselected.len(), 1);
    assert_eq!(view.unselected[0].work, key("second"));
    assert_eq!(input(&figures, &view.unselected[0].parts), Some(1000));
    let (outside, why) = attribution.outside(&selected(&["first"])).unwrap();
    assert_eq!(input(&figures, &outside), Some(11101));
    assert_eq!(
        reasons(&why),
        [
            "after last item",
            "before first item",
            "between items",
            "unselected item"
        ]
    );
}

// Claim: requests inside two interleaved items' windows are given to
// neither: they are unattributed as `overlap`, and the conserving layer
// stays the same when an allocation is asked for. An even allocation divides
// them between the two, the first item in key order taking the odd token,
// labelled `even`, after the item's whole segments. Rejects counting an
// overlapped request in both items, an allocation that changes the
// conserving layer, an uneven split, and parts out of the order they add up
// in.
#[test]
fn interleaved_items_leave_the_overlap_unattributed_unless_allocated_evenly() {
    let recording = Recording::of(&[("10:05", 10), ("10:15", 1001), ("10:25", 100000)]);
    let events = [
        event("first", "10:00"),
        event("first", "10:20"),
        event("second", "10:10"),
        event("second", "10:30"),
    ];
    let plan = Plan::new(&[], &events, None);
    let (plain, figures) = attribute(&recording, &plan, None);
    let layer = &plain.conserving;
    assert_eq!(
        input(&figures, &layer.figure(&selected(&["first"]))),
        Some(10)
    );
    assert_eq!(
        input(&figures, &layer.figure(&selected(&["second"]))),
        Some(100000)
    );
    let (unattributed, why) = plain.unattributed().unwrap();
    assert_eq!(reasons(&why), ["overlap"]);
    assert_eq!(input(&figures, &unattributed), Some(1001));
    assert!(plain.allocated.is_none());

    let (even, _) = attribute(&recording, &plan, Some(Allocation::Even));
    assert_eq!(even.conserving, plain.conserving);
    let allocated = even.layer(Take::Allocated(Allocation::Even)).unwrap();
    assert_eq!(
        input(&figures, &allocated.figure(&selected(&["first"]))),
        Some(10 + 501)
    );
    assert_eq!(
        input(&figures, &allocated.figure(&selected(&["second"]))),
        Some(100000 + 500)
    );
    assert!(allocated
        .shares()
        .iter()
        .any(|share| share.evidence.basis == Basis::Even));
    // Money adds up in order, so a figure lists its whole segments before
    // its even shares, whatever segment each comes from.
    let pieces: Vec<Piece> = allocated
        .figure(&selected(&["second"]))
        .iter()
        .map(|part| part.piece)
        .collect();
    assert_eq!(
        pieces,
        [
            Piece::Segment(3),
            Piece::Even {
                segment: 2,
                index: 1,
                shares: 2
            }
        ]
    );
    assert!(even.layer(Take::Allocated(Allocation::Following)).is_none());
}

// Claim: one executor bound to three items through three sequential runs is
// divided per run, and each item's share holds exactly its run; what follows
// the last run is the session's, after the last item. Rejects per-run
// figures absent for a session with several runs, and the trailing request
// in a run.
#[test]
fn runs_divide_a_warm_executor_per_run() {
    let recording = Recording::of(&[("10:05", 10), ("10:25", 100), ("10:45", 1000), ("11:30", 7)]);
    let spans = [
        run("one", "08:59", "10:10"),
        run("two", "10:20", "10:30"),
        run("three", "10:40", "10:50"),
    ];
    let plan = Plan::new(&spans, &[], None);
    let (attribution, figures) = attribute(&recording, &plan, None);
    let (requests, _) = recording.read(&plan);
    for (name, expected) in [("one", 10), ("two", 100), ("three", 1000)] {
        let layer = &attribution.conserving;
        assert_eq!(
            input(&figures, &layer.figure(&selected(&[name]))),
            Some(expected)
        );
        let parts = plan.run_share(&format!("run-{name}"), &requests);
        assert_eq!(input(&figures, &parts), Some(expected), "{name}");
    }
    assert!(attribution
        .conserving
        .shares()
        .iter()
        .all(|share| share.evidence
            == Evidence {
                tier: Tier::Bound,
                basis: Basis::Span
            }));
    let (_, why) = attribution.unattributed().unwrap();
    assert_eq!(reasons(&why), ["after last item"]);
    // Without windows, the estimate gives nothing beyond the conserving
    // shares.
    assert_eq!(attribution.estimated.parts, attribution.conserving.parts);
    assert_eq!(
        input(
            &figures,
            &attribution
                .conserving
                .figure(&selected(&["one", "two", "three"]))
        ),
        Some(1110)
    );
}

// Claim: a participant that starts each item and records its verdict gets a
// conserving share of each: its requests between the start and the verdict.
// Rejects a window one instant wide, which leaves the work unattributed.
#[test]
fn a_start_and_a_verdict_per_item_give_conserving_shares() {
    let recording = Recording::of(&[("10:05", 10), ("10:15", 100), ("10:35", 1000), ("10:45", 7)]);
    let events = [
        event("a", "10:00"),
        event("a", "10:20"),
        event("b", "10:30"),
        event("b", "10:50"),
    ];
    let plan = Plan::new(&[], &events, Some(&[at("10:05")]));
    let (attribution, figures) = attribute(&recording, &plan, None);
    let layer = &attribution.conserving;
    assert_eq!(input(&figures, &layer.figure(&selected(&["a"]))), Some(110));
    assert_eq!(
        input(&figures, &layer.figure(&selected(&["b"]))),
        Some(1007)
    );
}

// Claim: a participant that records only a verdict per item gets estimated
// shares from its working stretch: requests up to a verdict serve its item.
// They carry the `working-stretch` basis in their own layer, the conserving
// figure stays beside them, and conserving, estimated and unattributed parts
// add up to the whole. Rejects leaving the work unattributed, and mixing
// estimates into the conserving figure.
#[test]
fn verdicts_alone_give_working_stretch_shares_beside_the_conserving_figure() {
    let recording = Recording::of(&[("10:05", 10), ("10:15", 100), ("10:35", 1000), ("10:45", 7)]);
    let events = [event("a", "10:20"), event("b", "10:50")];
    let plan = Plan::new(&[], &events, Some(&[at("10:05")]));
    assert!(plan.estimates());
    let (attribution, figures) = attribute(&recording, &plan, None);
    let estimated = attribution.layer(Take::Estimated).unwrap();
    assert_eq!(
        input(&figures, &estimated.figure(&selected(&["a"]))),
        Some(110)
    );
    assert_eq!(
        input(&figures, &estimated.figure(&selected(&["b"]))),
        Some(1007)
    );
    assert_eq!(input(&figures, &estimated.unattributed()), Some(0));
    assert!(estimated.shares().iter().any(|share| share.evidence
        == Evidence {
            tier: Tier::Matched,
            basis: Basis::WorkingStretch
        }));
    assert_eq!(
        input(&figures, &attribution.conserving.figure(&selected(&["a"]))),
        Some(0)
    );
    assert_eq!(
        input(&figures, &estimated.figure(&selected(&["a", "b"]))),
        Some(1117)
    );
    let parts: Vec<Part> = estimated.parts.iter().map(|(part, _)| *part).collect();
    assert_eq!(total(&figures, &parts), recording.whole());
}

// Claim: a working stretch holding no event of the participant stays
// unattributed, while the stretch holding its verdicts is estimated; a
// shorter idle gap cuts the stretches finer, and the requests after a
// stretch's last event stay with that event's item. Rejects giving an
// idle-gapped stretch to the nearest verdict.
#[test]
fn a_stretch_with_no_event_stays_unattributed() {
    let recording = Recording::of(&[
        ("08:00", 5),
        ("08:10", 50),
        ("10:05", 10),
        ("10:15", 100),
        ("10:25", 3),
        ("10:50", 1000),
        ("10:55", 7),
    ]);
    let events = [event("a", "10:20"), event("b", "11:00")];
    let times: Vec<Instant> = recording
        .requests
        .iter()
        .filter_map(|(at, _)| *at)
        .collect();
    let estimate = |idle_minutes: i64| {
        let starts = stretch_starts(&times, idle_minutes);
        let plan = Plan::new(&[], &events, Some(&starts));
        let (attribution, figures) = attribute(&recording, &plan, None);
        let layer = attribution.estimated;
        (
            input(&figures, &layer.figure(&selected(&["a"]))),
            input(&figures, &layer.figure(&selected(&["b"]))),
            input(&figures, &layer.unattributed()),
        )
    };
    assert_eq!(estimate(45), (Some(110), Some(1010), Some(55)));
    // At a 15m idle gap, the 25 minutes after 10:25 end the first verdict's
    // stretch, so the request after that verdict stays with its item.
    assert_eq!(estimate(15), (Some(113), Some(1007), Some(55)));
}

// Claim: a following allocation gives what stays unattributed between two
// items, here a stretch with no event of the participant, to the next item,
// in its own layer beside the conserving and the estimated ones, labelled
// `following`; a stretch before the first item stays unattributed. Rejects
// adding it to the conserving or the estimated figure, and giving what
// precedes every item to the first.
#[test]
fn following_gives_what_remains_between_items_to_the_next() {
    let recording = Recording::of(&[
        ("08:00", 4000),
        ("10:05", 10),
        ("11:30", 20),
        ("12:50", 300),
    ]);
    let events = [event("a", "10:20"), event("b", "13:00")];
    let starts = [at("08:00"), at("10:05"), at("11:30"), at("12:50")];
    let plan = Plan::new(&[], &events, Some(&starts));
    let (attribution, figures) = attribute(&recording, &plan, Some(Allocation::Following));
    let b = selected(&["b"]);
    assert_eq!(input(&figures, &attribution.conserving.figure(&b)), Some(0));
    let estimated = attribution.layer(Take::Estimated).unwrap();
    assert_eq!(input(&figures, &estimated.figure(&b)), Some(300));
    let following = attribution
        .layer(Take::Allocated(Allocation::Following))
        .unwrap();
    assert_eq!(input(&figures, &following.figure(&b)), Some(320));
    assert_eq!(
        input(&figures, &following.figure(&selected(&["a"]))),
        Some(10)
    );
    let given: Vec<_> = following
        .shares()
        .into_iter()
        .filter(|share| share.evidence.basis == Basis::Following)
        .collect();
    assert_eq!(given.len(), 1);
    assert_eq!(given[0].work, key("b"));
    assert_eq!(input(&figures, &given[0].parts), Some(20));
    let before: Vec<_> = following
        .remainders()
        .into_iter()
        .filter(|remainder| remainder.reason == Reason::BeforeFirstItem)
        .collect();
    assert_eq!(before.len(), 1);
    assert_eq!(input(&figures, &before[0].parts), Some(4000));
}

// Claim: an executor's recording read as a participant's, with one event
// per item at each run's end, gets estimated shares equal to its exact run
// shares for sequential work with clean gaps. Rejects an estimator that
// drifts from the truth it can know.
#[test]
fn the_estimate_equals_exact_run_shares_for_sequential_work() {
    let recording = Recording::of(&[
        ("09:05", 11),
        ("09:25", 13),
        ("09:45", 170),
        ("10:05", 190),
        ("10:25", 2300),
        ("10:45", 2900),
    ]);
    let runs = [
        ("one", "09:00", "09:30"),
        ("two", "09:40", "10:10"),
        ("three", "10:20", "10:50"),
    ];
    let spans: Vec<Span<String>> = runs
        .iter()
        .map(|(work, from, to)| run(work, from, to))
        .collect();
    let exact = Plan::new(&spans, &[], None);
    let (attribution, figures) = attribute(&recording, &exact, None);
    let truth: Vec<Option<u64>> = runs
        .iter()
        .map(|(work, _, _)| input(&figures, &attribution.conserving.figure(&selected(&[work]))))
        .collect();
    assert_eq!(truth, [Some(24), Some(360), Some(5200)]);

    let events: Vec<Event<String>> = runs.iter().map(|(work, _, to)| event(work, to)).collect();
    let times: Vec<Instant> = recording
        .requests
        .iter()
        .filter_map(|(at, _)| *at)
        .collect();
    let read = Plan::new(&[], &events, Some(&stretch_starts(&times, 45)));
    let (attribution, figures) = attribute(&recording, &read, None);
    let estimated = attribution.estimated;
    let estimate: Vec<Option<u64>> = runs
        .iter()
        .map(|(work, _, _)| input(&figures, &estimated.figure(&selected(&[work]))))
        .collect();
    assert_eq!(estimate, truth);
}

/// The first stamp of each working stretch of `times`, in order: a gap above
/// `idle_minutes` starts a stretch.
fn stretch_starts(times: &[Instant], idle_minutes: i64) -> Vec<Instant> {
    let mut starts = Vec::new();
    let mut previous: Option<Instant> = None;
    for at in times {
        if previous.is_none_or(|previous| (*at - previous).num_minutes() > idle_minutes) {
            starts.push(*at);
        }
        previous = Some(*at);
    }
    starts
}

/// A small deterministic generator, so a failing case reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for index in (1..items.len()).rev() {
            let other = self.below(index as u64 + 1) as usize;
            items.swap(index, other);
        }
    }
}

const ITEMS: [&str; 4] = ["a", "b", "c", "d"];
const TIERS: [Tier; 3] = [Tier::Bound, Tier::Matched, Tier::Estimated];
const METHODS: [Option<Allocation>; 3] =
    [None, Some(Allocation::Even), Some(Allocation::Following)];

/// One generated session: its recording, its evidence, its working
/// stretches at a generated idle gap, and an allocation method.
struct Case {
    recording: Recording,
    spans: Vec<Span<String>>,
    events: Vec<Event<String>>,
    starts: Option<Vec<Instant>>,
    method: Option<Allocation>,
}

fn minute(rng: &mut Rng) -> Instant {
    at("08:00") + chrono::Duration::minutes(rng.below(240) as i64)
}

fn case(seed: u64) -> Case {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut requests: Vec<(Option<Instant>, Tokens)> = (0..rng.below(40))
        .map(|_| {
            let timestamp = (!rng.chance(10)).then(|| minute(&mut rng));
            let output = rng.below(50);
            (
                timestamp,
                Tokens {
                    input: Some(rng.below(5000)),
                    output: Some(output),
                    cache_read: None,
                    usd: None,
                },
            )
        })
        .collect();
    requests.sort_by_key(|(timestamp, _)| *timestamp);
    let pick = |rng: &mut Rng| key(ITEMS[rng.below(ITEMS.len() as u64) as usize]);
    let tier = |rng: &mut Rng| TIERS[rng.below(TIERS.len() as u64) as usize];
    let spans = (0..rng.below(5))
        .map(|index| {
            let from = minute(&mut rng);
            Span {
                work: pick(&mut rng),
                run: rng.chance(50).then(|| format!("run-{index}")),
                from,
                to: (!rng.chance(20))
                    .then(|| from + chrono::Duration::minutes(1 + rng.below(60) as i64)),
                tier: tier(&mut rng),
            }
        })
        .collect();
    let events = (0..rng.below(9))
        .map(|_| Event {
            at: minute(&mut rng),
            work: pick(&mut rng),
            tier: tier(&mut rng),
        })
        .collect();
    let idle = [10, 30, 45][rng.below(3) as usize];
    let times: Vec<Instant> = requests.iter().filter_map(|(at, _)| *at).collect();
    let starts = (!rng.chance(15)).then(|| stretch_starts(&times, idle));
    let method = METHODS[rng.below(METHODS.len() as u64) as usize];
    Case {
        recording: Recording { requests },
        spans,
        events,
        starts,
        method,
    }
}

impl Case {
    fn plan(&self) -> Plan<String> {
        Plan::new(&self.spans, &self.events, self.starts.as_deref())
    }

    fn layers(
        attribution: &Attribution<String>,
    ) -> Vec<&agent_tapes_core::attribution::Layer<String>> {
        std::iter::once(&attribution.conserving)
            .chain(std::iter::once(&attribution.estimated))
            .chain(&attribution.allocated)
            .collect()
    }
}

const CASES: u64 = 2000;

// Claim: in every layer, per counter, the items' shares and the remainders
// add up to the whole session, and every part of the read is in exactly one
// share or one remainder. Rejects a layer that drops untimed requests,
// counts an overlapped request twice, or splits a segment into shares that
// lose a token.
#[test]
fn every_layer_conserves_the_whole_per_counter() {
    // How many layers held requests in each kind of part, so the cases are
    // known to exercise every rule.
    let mut exercised: std::collections::BTreeMap<&str, usize> = Default::default();
    for seed in 0..CASES {
        let case = case(seed);
        let plan = case.plan();
        let (attribution, figures) = attribute(&case.recording, &plan, case.method);
        let whole = case.recording.whole();
        for layer in Case::layers(&attribution) {
            let shares = layer.shares();
            let remainders = layer.remainders();
            for share in &shares {
                if share.parts.iter().any(|part| part.requests > 0) {
                    *exercised.entry(share.evidence.basis.as_str()).or_default() += 1;
                }
            }
            for remainder in remainders.iter().filter(|remainder| remainder.requests > 0) {
                *exercised.entry(remainder.reason.as_str()).or_default() += 1;
            }
            let mut parts: Vec<Part> = shares
                .iter()
                .flat_map(|share| share.parts.iter().copied())
                .chain(
                    remainders
                        .iter()
                        .flat_map(|remainder| remainder.parts.iter().copied()),
                )
                .collect();
            // Untimed requests are listed where there are any; none is the
            // remainder's zero.
            if !remainders
                .iter()
                .any(|remainder| remainder.reason == Reason::RequestTimeUnrecorded)
            {
                assert_eq!(layer.parts.last().unwrap().0.requests, 0);
            }
            let summed = total(&figures, &parts);
            assert_eq!(
                (summed.input, summed.output),
                (whole.input, whole.output),
                "seed {seed} {:?}",
                layer.take
            );
            let mut conservation = Conservation::default();
            conservation.compare("input", summed.input, whole.input);
            conservation.compare("output", summed.output, whole.output);
            conservation.compare("cache_read", summed.cache_read, whole.cache_read);
            assert_eq!(conservation.status(), "conserved", "seed {seed}");
            let mut all: Vec<Part> = layer
                .parts
                .iter()
                .map(|(part, _)| *part)
                .filter(|part| part.requests > 0 || part.piece != Piece::Remainder)
                .collect();
            parts.sort_by_key(|part| part.piece);
            all.sort_by_key(|part| part.piece);
            assert_eq!(parts, all, "seed {seed} {:?}", layer.take);
        }
    }
    for kind in [
        "span",
        "event-window",
        "working-stretch",
        "even",
        "following",
        "overlap",
        "before first item",
        "between items",
        "after last item",
        "request time unrecorded",
    ] {
        assert!(
            exercised.get(kind).copied().unwrap_or(0) >= 50,
            "{kind}: {exercised:?}"
        );
    }
}

// Claim: the same spans and events in any order give the same plan and the
// same layers, events at one instant included. Rejects an engine whose
// result depends on the order the evidence arrives in.
#[test]
fn the_same_evidence_in_any_order_gives_the_same_attribution() {
    for seed in 0..CASES {
        let case = case(seed);
        let plan = case.plan();
        let (attribution, _) = attribute(&case.recording, &plan, case.method);
        let mut rng = Rng(seed | 1);
        let mut spans = case.spans.clone();
        let mut events = case.events.clone();
        for _ in 0..3 {
            rng.shuffle(&mut spans);
            rng.shuffle(&mut events);
            let shuffled = Plan::new(&spans, &events, case.starts.as_deref());
            assert_eq!(shuffled, plan, "seed {seed}");
            let (again, _) = attribute(&case.recording, &shuffled, case.method);
            assert_eq!(again, attribution, "seed {seed}");
        }
    }
}

// Claim: an item's share in every layer is the same whether it is selected
// alone or among others: a selection's figure is the sum of its items'
// figures alone, and an unselected item's share is reported apart, outside
// the figure, with the figure alone would give it. Rejects a selection that
// changes the partition, such as giving an overlap to the one selected item
// that covers it.
#[test]
fn an_items_share_does_not_depend_on_the_selection() {
    for seed in 0..CASES {
        let case = case(seed);
        let plan = case.plan();
        let (attribution, figures) = attribute(&case.recording, &plan, case.method);
        for layer in Case::layers(&attribution) {
            let alone = |name: &str| input(&figures, &layer.select(&selected(&[name])).figure);
            let every = layer.select(&selected(&ITEMS));
            let summed = ITEMS
                .iter()
                .map(|name| alone(name))
                .try_fold(0u64, |sum, part| Some(sum + part?));
            assert_eq!(input(&figures, &every.figure), summed, "seed {seed}");
            assert!(every.unselected.is_empty());
            for name in ITEMS {
                let others: Vec<&str> =
                    ITEMS.iter().copied().filter(|item| *item != name).collect();
                let view = layer.select(&selected(&others));
                let apart: Vec<Part> = view
                    .unselected
                    .iter()
                    .filter(|share| share.work == key(name))
                    .flat_map(|share| share.parts.iter().copied())
                    .collect();
                assert_eq!(input(&figures, &apart), alone(name), "seed {seed} {name}");
                assert!(view.unselected.iter().all(|share| share.work == key(name)));
            }
        }
        // The unselected item's conserving share is outside the view and
        // named for it.
        for name in ITEMS {
            let others: BTreeSet<String> = ITEMS
                .iter()
                .filter(|item| **item != name)
                .map(|item| key(item))
                .collect();
            let alone = attribution.conserving.figure(&selected(&[name]));
            let holds = alone.iter().any(|part| part.requests > 0);
            let outside = attribution.outside(&others);
            assert_eq!(
                outside
                    .as_ref()
                    .is_some_and(|(_, why)| why.contains(&Reason::UnselectedItem)),
                holds,
                "seed {seed} {name}"
            );
        }
    }
}

// Claim: a plan built from segments and windows a caller already holds, as
// a retained read keeps them, attributes like the plan the evidence builds.
// Rejects an engine that only works on evidence it cut itself.
#[test]
fn a_plan_from_retained_segments_attributes_like_a_fresh_one() {
    let recording = Recording::of(&[("10:05", 10), ("11:30", 20), ("12:50", 300)]);
    let events = [event("a", "10:20"), event("b", "13:00")];
    let starts = [at("10:05"), at("11:30"), at("12:50")];
    let fresh = Plan::new(&[], &events, Some(&starts));
    let retained = Plan {
        segments: fresh
            .segments
            .iter()
            .map(|segment| Segment {
                from: segment.from,
                to: segment.to,
                work: segment.work.clone(),
                runs: segment.runs.clone(),
            })
            .collect(),
        windows: fresh
            .windows
            .iter()
            .map(|window| Window {
                segment: window.segment,
                from: window.from,
                to: window.to,
                work: window.work.clone(),
            })
            .collect(),
    };
    let (requests, _) = recording.read(&fresh);
    assert_eq!(
        retained.attribute(&requests, Some(Allocation::Following), true),
        fresh.attribute(&requests, Some(Allocation::Following), true)
    );
    let short = Requests {
        segments: Vec::new(),
        ..requests
    };
    assert!(fresh.attribute(&short, None, true).is_none());
}

// Claim: a session whose whole in the segments' basis is not its own figure
// names the recorded total as unsplittable beside what no item holds.
// Rejects dividing a recorded total the segments do not hold.
#[test]
fn an_unsplittable_recorded_total_is_named() {
    let recording = Recording::of(&[("10:05", 10)]);
    let plan = Plan::new(&[span("a", "10:00", Some("10:10"))], &[], None);
    let (requests, _) = recording.read(&plan);
    let attribution = plan.attribute(&requests, None, false).unwrap();
    let (_, why) = attribution.unattributed().unwrap();
    assert_eq!(reasons(&why), ["recorded total not splittable"]);
    let whole = plan.attribute(&requests, None, true).unwrap();
    assert!(whole.unattributed().is_none());
    assert!(whole.outside(&selected(&["a"])).is_none());
    assert!(matches!(
        whole.conserving.parts[1].1,
        Owner::Item { ref work, .. } if work == &key("a")
    ));
}

// Claim: a share carries the tier of the evidence that gives it its item: a
// segment several pieces of one item's evidence cover takes the strongest,
// and an item's event window rests on all its events, so it takes the
// weakest of them. Rejects a share labelled stronger than what establishes
// it, and one weaker than the strongest evidence covering it.
#[test]
fn a_share_carries_the_tier_of_the_evidence_that_gives_it() {
    let recording = Recording::of(&[("10:15", 1), ("10:35", 10)]);
    let spans = [Span {
        tier: Tier::Bound,
        ..span("a", "10:00", Some("10:30"))
    }];
    let events = [
        Event {
            tier: Tier::Estimated,
            ..event("a", "10:10")
        },
        Event {
            tier: Tier::Matched,
            ..event("a", "10:40")
        },
    ];
    let plan = Plan::new(&spans, &events, None);
    let (attribution, figures) = attribute(&recording, &plan, None);
    let shares = attribution.conserving.shares();
    let held = |evidence: Evidence| {
        shares
            .iter()
            .find(|share| share.evidence == evidence)
            .map(|share| input(&figures, &share.parts))
    };
    assert_eq!(
        held(Evidence {
            tier: Tier::Bound,
            basis: Basis::Span
        }),
        Some(Some(1))
    );
    assert_eq!(
        held(Evidence {
            tier: Tier::Estimated,
            basis: Basis::EventWindow
        }),
        Some(Some(10))
    );
    assert_eq!(shares.len(), 2);
}
