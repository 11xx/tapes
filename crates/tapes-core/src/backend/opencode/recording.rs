//! Snapshot batches over complete native database parts. The first unfinished
//! part in the newest live message is the ordered stream's frontier. SQLite
//! scans/sorts under native indexes; response and process counts are bounded.
use super::*;
use crate::recording::{
    digest, Cursor, Direction, Item, Kind, Limits, Page, Parent, Position, Refusal, Status, Stop,
    Tool, PAGE_SCHEMA,
};
use std::sync::Arc;

const PART_BYTES: usize = MAX_COMMAND_BYTES as usize / 2;
const UNFINISHED: &str = "((json_extract(p.data,'$.type') IN ('text','reasoning') AND json_extract(m.data,'$.role')='assistant' AND coalesce(json_type(p.data,'$.time.end'),'null') NOT IN ('integer','real')) OR (json_extract(p.data,'$.type')='tool' AND coalesce(json_extract(p.data,'$.state.status'),'') NOT IN ('completed','error')))";

impl OpenCodeBackend {
    pub(crate) fn recording_page(
        &self,
        id: &str,
        direction: Direction<'_>,
        limits: &Limits,
    ) -> Result<Page> {
        if !self.uses_database() {
            return Err(Refusal::Unsupported {
                store: self.store.coordinate(),
            }
            .into());
        }
        let source = digest(format!("{}\0{id}", self.store.coordinate()).as_bytes());
        let sql_id = sql_literal(id);
        let mut bytes_read = 0;
        let backwards = !matches!(direction, Direction::Forwards(_));
        let cursor = match direction {
            Direction::Tail => None,
            Direction::Before(p) | Direction::Forwards(p) => Some(p.decode()?),
        };
        let bound = limits.scan_bytes.min(PART_BYTES);
        let mut inclusive = true;
        if let Some(cursor) = &cursor {
            if cursor.source != source || cursor.device != 0 || cursor.inode != 0 {
                return Err(Refusal::Replaced {
                    reason: "position belongs to another recording".to_owned(),
                }
                .into());
            }
            if !cursor.anchor.is_empty() {
                let rows = self.recording_query(&format!("SELECT {} AS row FROM part p JOIN message m ON m.id=p.message_id WHERE p.session_id={sql_id} AND p.id={} AND p.time_created={}", part_row(bound), sql_literal(&cursor.anchor), cursor.offset), &mut bytes_read)?;
                let Some(row) = rows.first() else {
                    return Err(Refusal::Changed {
                        reason: "positioned part removed".to_owned(),
                    }
                    .into());
                };
                source_bound(row, bound)?;
                let kinds = database_kinds(row)?;
                if cursor.part > kinds.len() {
                    return Err(Refusal::Changed {
                        reason: "positioned part shape changed".to_owned(),
                    }
                    .into());
                }
                inclusive = kinds.iter().enumerate().any(|(index, kind)| {
                    limits.wants(*kind)
                        && if backwards {
                            index < cursor.part
                        } else {
                            index >= cursor.part
                        }
                });
                if cursor.record.as_deref() != Some(&digest(row["data"].to_string().as_bytes())) {
                    return Err(Refusal::Changed {
                        reason: "positioned part changed".to_owned(),
                    }
                    .into());
                }
            }
        }
        let empty = Position::encode(&Cursor {
            version: 1,
            source: source.clone(),
            device: 0,
            inode: 0,
            offset: 0,
            part: 0,
            anchor: String::new(),
            record: None,
        })?;
        let mut start = match direction {
            Direction::Before(p) | Direction::Forwards(p) => p.clone(),
            _ => empty,
        };
        let mut end = start.clone();
        let mut past = String::new();
        if let Some(cursor) = &cursor {
            if !cursor.anchor.is_empty() {
                let cmp = if backwards { "<" } else { ">" };
                let equal = if inclusive { "=" } else { "" };
                past = format!(
                    " AND (p.time_created {cmp} {} OR (p.time_created={} AND p.id {cmp}{equal} {}))",
                    cursor.offset,
                    cursor.offset,
                    sql_literal(&cursor.anchor)
                );
            } else if backwards {
                past = " AND 0".to_owned();
            }
        }
        let order = if backwards { "DESC" } else { "ASC" };
        let rows_limit = limits
            .items
            .saturating_add(1)
            .min(limits.scan_bytes / 32 + 1);
        let filter = kind_filter(limits);
        let pending_rule = format!("{UNFINISHED} AND m.id=(SELECT id FROM message WHERE session_id=p.session_id ORDER BY time_created DESC,id DESC LIMIT 1) AND coalesce(json_type(m.data,'$.time.completed'),'null')='null' AND coalesce(json_type(m.data,'$.error'),'null')='null'");
        let query = format!(
            "WITH pending AS (SELECT p.id,p.time_created FROM part p JOIN message m ON m.id=p.message_id AND m.session_id=p.session_id WHERE p.session_id={sql_id} AND {pending_rule} ORDER BY p.time_created,p.id LIMIT 1), \
             selected AS (SELECT {} AS wire, length(CAST(p.data AS BLOB))+length(p.id)+length(p.message_id)+512 AS cost, p.time_created AS t, p.id AS k FROM part p JOIN message m ON m.id=p.message_id AND m.session_id=p.session_id \
             WHERE p.session_id={sql_id} AND typeof(p.time_created)='integer' AND p.time_created>=0 AND typeof(p.id)='text'{past} AND (NOT EXISTS(SELECT 1 FROM pending) OR p.time_created<(SELECT time_created FROM pending) OR (p.time_created=(SELECT time_created FROM pending) AND p.id<(SELECT id FROM pending))){filter} \
             ORDER BY p.time_created {order},p.id {order} LIMIT {rows_limit}), \
             sized AS (SELECT *,sum(cost) OVER(ORDER BY t {order},k {order}) AS total,row_number() OVER(ORDER BY t {order},k {order}) AS n FROM selected), \
             retained AS (SELECT * FROM sized WHERE total<={bound} OR n=1) \
             SELECT json_object('state',json_object('parent',(SELECT parent_id FROM session WHERE id={sql_id}),'pending',(SELECT id FROM pending),'scan_limited',(SELECT count(*) FROM retained)<(SELECT count(*) FROM selected),'more',(SELECT count(*) FROM selected)={rows_limit})) AS row \
             UNION ALL SELECT wire AS row FROM (SELECT * FROM retained ORDER BY n)", part_row(bound));
        let mut rows = self.recording_query(&query, &mut bytes_read)?;
        let state = rows.remove(0)["state"].clone();
        let pending = state["pending"].as_str().map(str::to_owned);
        let mut parents = state["parent"]
            .as_str()
            .map(|p| Parent {
                relation: "child-of".to_owned(),
                reference: p.to_owned(),
            })
            .into_iter()
            .collect::<Vec<_>>();
        // An optional native fork column is read independently of part batches.
        if let Ok(forks) = self.recording_query(
            &format!(
                "SELECT json_object('fork',fork_session_id) AS row FROM session WHERE id={sql_id}"
            ),
            &mut bytes_read,
        ) {
            parents.extend(forks.iter().filter_map(|row| {
                row["fork"].as_str().map(|p| Parent {
                    relation: "forked-from".to_owned(),
                    reference: p.to_owned(),
                })
            }));
        }
        let mut items = Vec::new();
        let mut item_bytes: usize = 0;
        let mut observed = Vec::new();
        let mut stop = if pending.is_some() && !matches!(direction, Direction::Before(_)) {
            Stop::Pending
        } else {
            Stop::End
        };
        let mut full = false;
        for row in &rows {
            source_bound(row, bound)?;
            if row["data"].is_null() {
                anyhow::bail!("malformed OpenCode part");
            }
            let key = RowKey::from_row(row)?;
            let hash = digest(row["data"].to_string().as_bytes());
            let point = Cursor {
                version: 1,
                source: source.clone(),
                device: 0,
                inode: 0,
                offset: key.time_created as u64,
                part: 0,
                anchor: key.id.clone(),
                record: Some(hash.clone()),
            };
            let candidates = database_items(row, &point)?;
            let count = candidates.len();
            let index = cursor
                .as_ref()
                .filter(|c| c.anchor == key.id)
                .map_or(if backwards { count } else { 0 }, |c| c.part);
            if index > count {
                return Err(Refusal::Changed {
                    reason: "positioned part shape changed".to_owned(),
                }
                .into());
            }
            if matches!(direction, Direction::Tail) && observed.is_empty() {
                let mut last = point.clone();
                last.part = count;
                end = Position::encode(&last)?;
            }
            observed.push((key.id, hash));
            let indices: Vec<usize> = if backwards {
                (0..index).rev().collect()
            } else {
                (index..count).collect()
            };
            for index in indices {
                if !limits.wants(candidates[index].kind) {
                    continue;
                }
                let item = &candidates[index];
                let size = item.budget_bytes()?;
                if size > limits.bytes && items.is_empty() {
                    return Err(Refusal::Oversized {
                        needed: size,
                        start: item.start.clone(),
                        end: item.end.clone(),
                    }
                    .into());
                }
                if item_bytes.saturating_add(size) > limits.bytes {
                    stop = Stop::Bytes;
                    full = true;
                    break;
                }
                item_bytes += size;
                if backwards {
                    start = item.start.clone();
                } else {
                    end = item.end.clone();
                }
                items.push(item.clone());
                if items.len() == limits.items {
                    stop = Stop::Items;
                    full = true;
                    break;
                }
            }
            if full {
                break;
            }
            let mut boundary = point;
            boundary.part = if backwards { 0 } else { count };
            if backwards {
                start = Position::encode(&boundary)?;
            } else {
                end = Position::encode(&boundary)?;
            }
        }
        if !full && (state["scan_limited"] == true || state["more"] == true) {
            stop = Stop::ScanBytes;
        }
        if !observed.is_empty() {
            let ids = observed
                .iter()
                .map(|(id, _)| sql_literal(id))
                .collect::<Vec<_>>()
                .join(",");
            let checked=self.recording_query(&format!("SELECT json_object('id',id,'data',json(data)) AS row FROM part WHERE session_id={sql_id} AND id IN ({ids})"), &mut bytes_read)?;
            for (id, hash) in &observed {
                if !checked
                    .iter()
                    .any(|r| r["id"] == *id && digest(r["data"].to_string().as_bytes()) == *hash)
                {
                    return Err(Refusal::Changed {
                        reason: "part changed during page read".to_owned(),
                    }
                    .into());
                }
            }
        }
        if backwards {
            items.reverse();
        }
        Ok(Page { schema: PAGE_SCHEMA, recording: id.to_owned(), harness: "opencode".to_owned(), parents, items, start, end, stop, item_bytes, bytes_read, records_read: observed.len(), malformed_records:0, pending_bytes:0, pending, notes:vec!["Complete parts in time_created,id order. Unfinished parts in the newest message without completion/error stop the stream; older incomplete parts have never-finished status. A page captures one batch and verifies it in one query; optional parent/fork and cursor validation add constant query costs. SQLite scans/sorts the session under its native indexes; bytes_read measures SQL responses.".to_owned()] })
    }
    fn recording_query(&self, sql: &str, bytes: &mut usize) -> Result<Vec<Value>> {
        let text = self.database_text(sql)?;
        *bytes += text.len();
        strict_database_rows(parse_database_rows_lossy(&text)?)
    }
}
fn source_bound(row: &Value, bound: usize) -> Result<()> {
    if row["bytes"].as_u64().unwrap_or(0) > bound as u64 {
        return Err(Refusal::ScanBudget {
            limit: bound,
            needed: row["bytes"].as_u64().and_then(|n| usize::try_from(n).ok()),
        }
        .into());
    }
    Ok(())
}
fn part_row(bound: usize) -> String {
    format!("json_object('id',p.id,'time_created',p.time_created,'message_id',p.message_id,'parent_id',json_extract(m.data,'$.parentID'),'role',json_extract(m.data,'$.role'),'model',json_extract(m.data,'$.modelID'),'variant',json_extract(m.data,'$.variant'),'unfinished',{UNFINISHED},'bytes',length(CAST(p.data AS BLOB)),'data',CASE WHEN length(CAST(p.data AS BLOB))<={bound} AND json_valid(p.data) THEN json(p.data) END)")
}
fn kind_filter(limits: &Limits) -> String {
    if limits.kinds.is_empty() {
        return String::new();
    }
    let mut p = Vec::new();
    for kind in &limits.kinds {
        p.push(match kind {
            Kind::User=>"json_extract(p.data,'$.type')='text' AND json_extract(m.data,'$.role')='user'",
            Kind::Assistant=>"json_extract(p.data,'$.type')='text' AND json_extract(m.data,'$.role')='assistant'",
            Kind::Ambient=>"json_extract(p.data,'$.type')='text' AND json_extract(m.data,'$.role') IN ('system','developer')",
            Kind::Reasoning=>"json_extract(p.data,'$.type')='reasoning'",
            Kind::ToolCall=>"json_extract(p.data,'$.type')='tool'",
            Kind::ToolResult=>"json_extract(p.data,'$.type')='tool' AND json_extract(p.data,'$.state.status') IN ('completed','error')",
            Kind::Unknown=>"coalesce(json_extract(p.data,'$.type'),'') NOT IN ('text','reasoning','tool')",
            _=>"0",
        });
    }
    format!(
        " AND ({})",
        p.into_iter()
            .map(|s| format!("({s})"))
            .collect::<Vec<_>>()
            .join(" OR ")
    )
}
fn database_kinds(row: &Value) -> Result<Vec<Kind>> {
    Ok(match row["data"]["type"].as_str() {
        Some("text") => vec![match row["role"].as_str() {
            Some("user") => Kind::User,
            Some("assistant") => Kind::Assistant,
            Some("system" | "developer") => Kind::Ambient,
            _ => anyhow::bail!("OpenCode message has no recognized role"),
        }],
        Some("reasoning") => vec![Kind::Reasoning],
        Some("tool") => {
            if matches!(
                row["data"]["state"]["status"].as_str(),
                Some("completed" | "error")
            ) {
                vec![Kind::ToolCall, Kind::ToolResult]
            } else {
                vec![Kind::ToolCall]
            }
        }
        _ => vec![Kind::Unknown],
    })
}
fn database_items(row: &Value, point: &Cursor) -> Result<Vec<Item>> {
    let content = Arc::new(row["data"].clone());
    let role = match row["role"].as_str() {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        Some("system") => Role::System,
        Some("developer") => Role::Developer,
        _ => anyhow::bail!("OpenCode message has no recognized role"),
    };
    let kinds = database_kinds(row)?;
    let mut items = Vec::new();
    for (index, kind) in kinds.into_iter().enumerate() {
        let mut before = point.clone();
        before.part = index;
        let mut after = point.clone();
        after.part = index + 1;
        let (tool, call_id) = if matches!(kind, Kind::ToolCall | Kind::ToolResult) {
            let (mut t, c) = Tool::native(&content, "opencode", kind);
            if row["unfinished"] == 1 {
                t.status = Some(Status::NeverFinished);
            }
            (Some(t), c)
        } else {
            (None, None)
        };
        items.push(Item {
            start: Position::encode(&before)?,
            end: Position::encode(&after)?,
            kind,
            role: if tool.is_some() {
                Role::Tool
            } else if kind == Kind::Reasoning {
                Role::Reasoning
            } else {
                role.clone()
            },
            text: if matches!(
                kind,
                Kind::User | Kind::Assistant | Kind::Ambient | Kind::Reasoning
            ) {
                content["text"].as_str().unwrap_or("").to_owned()
            } else {
                String::new()
            },
            ts: epoch_millis(&content["time"]["start"])
                .or_else(|| epoch_millis(&row["time_created"])),
            native_id: row["id"].as_str().map(str::to_owned),
            request_turn_id: None,
            model: row["model"].as_str().map(|id| Model {
                id: id.to_owned(),
                variant: row["variant"].as_str().map(str::to_owned),
            }),
            origin: None,
            call_id,
            tool,
            status: if matches!(kind, Kind::Assistant | Kind::Reasoning) {
                Some(if row["unfinished"] == 1 {
                    Status::NeverFinished
                } else {
                    Status::Completed
                })
            } else {
                None
            },
            parent_id: row["parent_id"].as_str().map(str::to_owned),
            message_id: row["message_id"].as_str().map(str::to_owned),
            representation: None,
            content: content.clone(),
        });
    }
    Ok(items)
}
