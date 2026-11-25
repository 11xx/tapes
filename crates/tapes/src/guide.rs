//! The guide: what a session needs before its first command, and the depth
//! behind it.
//!
//! Bare `tapes` prints the map, which is bounded so that it is read whole.
//! `tapes guide` indexes the topics and prints one at a time; each topic is
//! bounded too. Anything that belongs to one command's contract stays in that
//! command's `--help`.

use anyhow::Result;
use clap::Command;

/// What bare `tapes` prints.
pub const MAP: &str = include_str!("guide/map.txt");

/// The matching lines a search shows for one route before it only counts them.
const SEARCH_CONTEXT_LINES: usize = 3;

pub struct Topic {
    pub name: &'static str,
    pub summary: &'static str,
    pub body: &'static str,
}

pub struct Group {
    pub title: &'static str,
    pub topics: &'static [Topic],
}

macro_rules! topic {
    ($name:literal, $summary:literal) => {
        Topic {
            name: $name,
            summary: $summary,
            body: include_str!(concat!("guide/topics/", $name, ".txt")),
        }
    };
}

pub const GROUPS: &[Group] = &[
    Group {
        title: "Continue a session's work",
        topics: &[
            topic!(
                "continue",
                "What a continuation needs, and where a long session goes"
            ),
            topic!(
                "workset",
                "Freeze a session and its relatives into lanes; check what they cite"
            ),
            topic!(
                "export",
                "The three bundle files and the order to read them in"
            ),
            topic!(
                "endings",
                "What sessions end on, and the signs that a read is partial"
            ),
        ],
    },
    Group {
        title: "Find and read sessions",
        topics: &[
            topic!(
                "find",
                "--latest, listing, search, launch mode and recorded titles"
            ),
            topic!("t3", "Sessions that T3 Code threads run on"),
            topic!(
                "show",
                "Windows, kinds, locations, timelines and turn ordinals"
            ),
            topic!(
                "read",
                "Pages of typed items by position, for a growing recording"
            ),
            topic!(
                "history",
                "Older history pages, history search and pinned reads"
            ),
            topic!(
                "events",
                "Typed tool calls and results, across one session or many"
            ),
            topic!(
                "lineage",
                "Relatives a recording names, and Claude child recordings"
            ),
            topic!(
                "stats",
                "Counts of what a recording holds, and tools over a selection"
            ),
        ],
    },
    Group {
        title: "Usage and cost",
        topics: &[
            topic!("usage", "Where one session's quota went, and spans of it"),
            topic!(
                "usage-summary",
                "Summing a selection, and sessions that overlap"
            ),
            topic!(
                "usage-window",
                "Consumption inside a time window, and a parent's share"
            ),
            topic!(
                "usage-quota",
                "Quota windows, allowance rates and forecasts"
            ),
            topic!(
                "usage-counters",
                "What each counter covers and when it is complete"
            ),
            topic!(
                "usage-branches",
                "Pi's abandoned branches and Claude reconciliation"
            ),
            topic!("usage-family", "A session and everything it caused"),
            topic!(
                "usage-keys",
                "Pi consumption event keys and Codex request series"
            ),
            topic!("change-cost", "What a set of commits cost"),
            topic!("prices", "Fetching and keeping a price table"),
            topic!(
                "price-tables",
                "Price table formats, plans and list-price estimates"
            ),
        ],
    },
    Group {
        title: "Other sources and launchers",
        topics: &[
            topic!("input", "ChatGPT and Perplexity exports"),
            topic!("remote", "Another machine's sessions over ssh"),
            topic!(
                "native-store",
                "A copied Antigravity, Claude, Codex or Pi store"
            ),
            topic!(
                "launch",
                "How a launcher declares which session started which"
            ),
        ],
    },
];

fn topics() -> impl Iterator<Item = &'static Topic> {
    GROUPS.iter().flat_map(|group| group.topics.iter())
}

/// Every topic name, for the argument parser to accept and to list.
pub fn topic_names() -> Vec<&'static str> {
    topics().map(|topic| topic.name).collect()
}

pub fn print_map() {
    print!("{MAP}");
}

fn print_index() {
    println!("Guide topics, one at a time with `tapes guide <topic>`:");
    for group in GROUPS {
        println!("\n{}", group.title);
        for topic in group.topics {
            println!("  tapes guide {:<16} {}", topic.name, topic.summary);
        }
    }
    println!(
        "\n  tapes guide --search TEXT    The topics and command helps that mention TEXT.\n  \
         tapes guide --all            Every topic at once: long, only when asked for.\n  \
         tapes <command> --help       A command's full contract."
    );
}

fn print_all() {
    for (index, topic) in topics().enumerate() {
        if index > 0 {
            println!();
        }
        print!("{}", topic.body);
    }
}

/// A place the guide's text lives, and the command that prints it.
struct Route {
    command: String,
    text: String,
}

/// The map, every topic, and every command's rendered `--help`.
fn routes(root: &Command) -> Vec<Route> {
    let mut routes = vec![Route {
        command: "tapes".to_owned(),
        text: MAP.to_owned(),
    }];
    routes.extend(topics().map(|topic| Route {
        command: format!("tapes guide {}", topic.name),
        text: topic.body.to_owned(),
    }));
    let mut root = root.clone();
    root.build();
    help_routes(&root, "tapes", &mut routes);
    routes
}

fn help_routes(command: &Command, path: &str, routes: &mut Vec<Route>) {
    for sub in command.get_subcommands() {
        if sub.get_name() == "help" {
            continue;
        }
        let path = format!("{path} {}", sub.get_name());
        routes.push(Route {
            command: format!("{path} --help"),
            text: sub.clone().render_long_help().to_string(),
        });
        help_routes(sub, &path, routes);
    }
}

/// List each route whose text holds `needle`, with its first matching lines.
/// The match is literal and ignores case. No route's whole text is printed.
fn search(root: &Command, needle: &str) -> Result<()> {
    anyhow::ensure!(!needle.trim().is_empty(), "--search needs text to look for");
    let wanted = needle.to_lowercase();
    let mut found = 0;
    for route in routes(root) {
        let matching: Vec<&str> = route
            .text
            .lines()
            .filter(|line| line.to_lowercase().contains(&wanted))
            .collect();
        if matching.is_empty() {
            continue;
        }
        found += 1;
        println!("{}", route.command);
        for line in matching.iter().take(SEARCH_CONTEXT_LINES) {
            println!("    {}", excerpt(line.trim(), &wanted));
        }
        if matching.len() > SEARCH_CONTEXT_LINES {
            println!(
                "    … {} more matching lines",
                matching.len() - SEARCH_CONTEXT_LINES
            );
        }
    }
    if found == 0 {
        println!(
            "No guide topic or command help mentions `{needle}`. `tapes guide` lists the topics."
        );
    }
    Ok(())
}

/// One line of search context. A long line is cut to a window around the
/// match, on character boundaries, with an ellipsis at each cut end.
fn excerpt(line: &str, wanted: &str) -> String {
    const WIDTH: usize = 160;
    let characters: Vec<char> = line.chars().collect();
    if characters.len() <= WIDTH {
        return line.to_owned();
    }
    let lowered = line.to_lowercase();
    let at = lowered
        .find(wanted)
        .map_or(0, |byte| lowered[..byte].chars().count());
    let start = at.saturating_sub(WIDTH / 4).min(characters.len() - WIDTH);
    let window: String = characters[start..start + WIDTH].iter().collect();
    let before = if start > 0 { "…" } else { "" };
    let after = if start + WIDTH < characters.len() {
        "…"
    } else {
        ""
    };
    format!("{before}{window}{after}")
}

/// Answer `tapes guide`: the index, one topic, a search, or every topic.
pub fn run(root: &Command, topic: Option<&str>, all: bool, needle: Option<&str>) -> Result<()> {
    if let Some(needle) = needle {
        return search(root, needle);
    }
    if all {
        print_all();
        return Ok(());
    }
    match topic {
        None => print_index(),
        Some(name) => {
            let topic = topics()
                .find(|topic| topic.name == name)
                .expect("the argument parser admits only topic names");
            print!("{}", topic.body);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The most lines the map may hold.
    const MAP_MAX_LINES: usize = 80;
    /// The most bytes the map may hold.
    const MAP_MAX_BYTES: usize = 6_144;
    /// The most bytes one topic may hold. A topic that outgrows it is split.
    const TOPIC_MAX_BYTES: usize = 16_384;

    /// A reader who prints the map gets all of it in one modest read. Rejects
    /// a map that grows until readers cut it short and lose its end.
    #[test]
    fn the_map_fits_one_reading() {
        assert!(
            MAP.lines().count() <= MAP_MAX_LINES,
            "{} lines",
            MAP.lines().count()
        );
        assert!(MAP.len() <= MAP_MAX_BYTES, "{} bytes", MAP.len());
    }

    /// One topic is one bounded read. Rejects a topic that has grown into a
    /// manual of its own instead of being split.
    #[test]
    fn every_topic_fits_its_bound() {
        for topic in topics() {
            assert!(
                topic.body.len() <= TOPIC_MAX_BYTES,
                "`{}` holds {} bytes; split it",
                topic.name,
                topic.body.len()
            );
            assert!(
                topic.body.ends_with('\n'),
                "`{}` must end with a newline",
                topic.name
            );
        }
    }

    /// Topic names a `Topics:` line of the map lists, to its end.
    fn topics_the_map_names() -> BTreeSet<&'static str> {
        let mut named = BTreeSet::new();
        let mut listing = false;
        for line in MAP.lines() {
            let line = line.trim();
            let rest = match line.strip_prefix("Topics:") {
                Some(rest) => {
                    listing = true;
                    rest
                }
                None if listing && !line.is_empty() && line == line.to_lowercase() => line,
                None => {
                    listing = false;
                    continue;
                }
            };
            named.extend(rest.split_whitespace());
        }
        named
    }

    /// The map routes to every topic and to nothing else. Rejects a topic no
    /// reader of the map can discover, and a route that names no topic.
    #[test]
    fn the_map_names_every_topic_and_only_topics() {
        let known: BTreeSet<&str> = topics().map(|topic| topic.name).collect();
        assert_eq!(known.len(), topics().count(), "a topic name repeats");
        assert_eq!(topics_the_map_names(), known);
    }

    /// A topic that sends its reader to another topic names one that exists.
    /// Rejects a cross-reference left pointing at a renamed or removed topic.
    #[test]
    fn every_cross_reference_names_a_topic() {
        let known: BTreeSet<&str> = topics().map(|topic| topic.name).collect();
        for topic in topics() {
            for (at, _) in topic.body.match_indices("`tapes guide ") {
                let rest = &topic.body[at + "`tapes guide ".len()..];
                let name = &rest[..rest.find('`').expect("a closed reference")];
                assert!(known.contains(name), "`{}` refers to `{name}`", topic.name);
            }
        }
    }
}
