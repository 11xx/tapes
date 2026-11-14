//! Keyset pages of complete native database parts. Mutable part contents are
//! checked on resume and again before publishing a page. SQLite may scan/sort
//! rows internally: response bounds do not promise database I/O bounds.
use super::*;
use crate::recording::{
    complete_tool, digest, turn_kind, Cursor, Direction, Item, Kind, Limits, Page, Parent,
    Position, Stop, PAGE_SCHEMA,
};

impl OpenCodeBackend {
    pub(crate) fn recording_page(
        &self,
        id: &str,
        direction: Direction<'_>,
        limits: &Limits,
    ) -> Result<Page> {
        anyhow::ensure!(self.uses_database(), "OpenCode API recordings have mutable messages and no supported stateless item positions; use the database store");
        let source = digest(format!("{}\0{id}", self.store.coordinate()).as_bytes());
        let mut bytes_read = 0;
        let sql_id = sql_literal(id);
        let parents = self.recording_query(&format!("SELECT json_object('parent_id', parent_id) AS row FROM session WHERE id = {sql_id}"), &mut bytes_read)?;
        let mut parents: Vec<Parent> = parents
            .iter()
            .filter_map(|row| {
                row["parent_id"].as_str().map(|parent| Parent {
                    relation: "child-of".to_owned(),
                    reference: parent.to_owned(),
                })
            })
            .collect();
        // Fork columns are optional in the native schema.
        if let Ok(rows) = self.recording_query(&format!("SELECT json_object('fork', fork_session_id) AS row FROM session WHERE id = {sql_id}"), &mut bytes_read) {
            parents.extend(rows.iter().filter_map(|row| row["fork"].as_str().map(|parent| Parent { relation: "forked-from".to_owned(), reference: parent.to_owned() })));
        }
        let backwards = !matches!(direction, Direction::Forwards(_));
        let mut current = match direction {
            Direction::Tail => None,
            Direction::Before(position) | Direction::Forwards(position) => {
                let cursor = position.decode()?;
                anyhow::ensure!(
                    cursor.source == source && cursor.device == 0 && cursor.inode == 0,
                    "position belongs to another recording"
                );
                if !cursor.anchor.is_empty() {
                    let row = self
                        .recording_row(
                            &sql_id,
                            &format!(
                                " AND p.id = {} AND p.time_created = {}",
                                sql_literal(&cursor.anchor),
                                cursor.offset
                            ),
                            "ASC",
                            limits,
                            false,
                            &mut bytes_read,
                        )?
                        .context("OpenCode positioned part was removed")?;
                    anyhow::ensure!(
                        cursor.record.as_deref()
                            == Some(&digest(row["data"].to_string().as_bytes())),
                        "OpenCode positioned part changed; reopen at the tail"
                    );
                }
                Some(cursor)
            }
        };
        let empty = || {
            Position::encode(&Cursor {
                version: 1,
                source: source.clone(),
                device: 0,
                inode: 0,
                offset: 0,
                part: 0,
                anchor: String::new(),
                record: None,
            })
        };
        let mut start = current
            .as_ref()
            .map(Position::encode)
            .transpose()?
            .unwrap_or(empty()?);
        let mut end = start.clone();
        let mut items = Vec::new();
        let mut item_bytes: usize = 0;
        let mut records_read = 0;
        let mut observed = Vec::new();
        let mut stop = Stop::End;
        let mut inclusive = true;
        loop {
            if items.len() == limits.items {
                stop = Stop::Items;
                break;
            }
            if bytes_read >= limits.scan_bytes && records_read > 0 {
                stop = Stop::ScanBytes;
                break;
            }
            if backwards
                && current
                    .as_ref()
                    .is_some_and(|cursor| cursor.anchor.is_empty())
            {
                break;
            }
            let past = match &current {
                Some(cursor) if !cursor.anchor.is_empty() => {
                    let time = cursor.offset;
                    let id = sql_literal(&cursor.anchor);
                    let comparison = if backwards { "<" } else { ">" };
                    let equal = if inclusive { "=" } else { "" };
                    format!(" AND (p.time_created {comparison} {time} OR (p.time_created = {time} AND p.id {comparison}{equal} {id}))")
                }
                _ => String::new(),
            };
            let row = self.recording_row(
                &sql_id,
                &past,
                if backwards { "DESC" } else { "ASC" },
                limits,
                true,
                &mut bytes_read,
            )?;
            let Some(row) = row else {
                break;
            };
            records_read += 1;
            let key = RowKey::from_row(&row)?;
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
            let mut content = row["data"].clone();
            if content.is_null() {
                anyhow::bail!("OpenCode part {} is malformed", key.id);
            }
            content["id"] = json!(key.id);
            let mut message = json!({"id":row["message_id"],"type":row["role"],"time":{"created":row["message_time"]},"content":[content],"text":content["text"]});
            if message["time"]["created"].is_null() {
                message["time"]["created"] = json!(key.time_created);
            }
            let turns = parse_message(&message);
            let mut candidates = Vec::new();
            for mut turn in turns {
                if turn.ts.is_none() {
                    turn.ts = epoch_millis(&json!(key.time_created));
                }
                let call_id = turn.tool.as_ref().and_then(|tool| tool.call_id.clone());
                let kinds = if turn.kind == TurnKind::Tool {
                    let mut kinds = vec![Kind::ToolCall];
                    if matches!(
                        content["state"]["status"].as_str(),
                        Some("completed" | "error")
                    ) {
                        kinds.push(Kind::ToolResult);
                    }
                    kinds
                } else {
                    vec![turn_kind(turn.kind)]
                };
                for kind in kinds {
                    candidates.push((kind, turn.clone(), call_id.clone()));
                }
            }
            let count = candidates.len();
            let index = current
                .as_ref()
                .filter(|c| inclusive && c.anchor == key.id)
                .map_or(if backwards { count } else { 0 }, |c| c.part);
            anyhow::ensure!(index <= count, "OpenCode position part outside record");
            let indices: Vec<usize> = if backwards {
                (0..index).rev().collect()
            } else {
                (index..count).collect()
            };
            let mut full = false;
            for index in indices {
                let (kind, turn, call_id) = &candidates[index];
                if !limits.wants(*kind) {
                    continue;
                }
                let mut before = point.clone();
                before.part = index;
                let mut after = point.clone();
                after.part = index + 1;
                let item = Item {
                    position: Position::encode(&before)?,
                    end: Position::encode(&after)?,
                    kind: *kind,
                    tool: complete_tool(turn, &row["data"], "opencode", *kind),
                    turn: turn.clone(),
                    content: row["data"].clone(),
                    call_id: call_id.clone(),
                    parent_id: row["message_id"].as_str().map(str::to_owned),
                    representation: None,
                };
                let size = serde_json::to_vec(&item)?.len();
                if size > limits.bytes && items.is_empty() {
                    anyhow::bail!("item requires {size} bytes; increase item byte budget ({}); content was not clipped", limits.bytes);
                }
                if item_bytes.saturating_add(size) > limits.bytes {
                    stop = Stop::Bytes;
                    full = true;
                    break;
                }
                item_bytes += size;
                if backwards {
                    start = item.position.clone();
                } else {
                    end = item.end.clone();
                }
                items.push(item);
                if items.len() == limits.items {
                    stop = Stop::Items;
                    full = true;
                    break;
                }
            }
            // Every emitted row is checked once more before publishing.
            observed.push((key.id.clone(), hash));
            if matches!(direction, Direction::Tail) && records_read == 1 {
                let mut last = point.clone();
                last.part = count;
                end = Position::encode(&last)?;
            }
            if full {
                break;
            }
            current = Some(point.clone());
            inclusive = false;
            let mut boundary = point;
            boundary.part = if backwards { 0 } else { count };
            if backwards {
                start = Position::encode(&boundary)?;
            } else {
                end = Position::encode(&boundary)?;
            }
        }
        for (id, hash) in observed {
            let row = self
                .recording_row(
                    &sql_id,
                    &format!(" AND p.id = {}", sql_literal(&id)),
                    "ASC",
                    limits,
                    false,
                    &mut bytes_read,
                )?
                .context("OpenCode part removed during page read")?;
            anyhow::ensure!(
                digest(row["data"].to_string().as_bytes()) == hash,
                "OpenCode part changed during page read"
            );
        }
        if backwards {
            items.reverse();
        }
        Ok(Page { schema: PAGE_SCHEMA, recording: id.to_owned(), harness: "opencode".to_owned(), parents, items, start, end, stop, item_bytes, bytes_read, records_read, malformed_records: 0, pending_bytes: 0, notes: vec!["Database part order is time_created,id. Parts are mutable: a changed positioned part refuses; reopen at the tail to refresh it. SQL response bytes are measured; SQLite internal scan/sort I/O is not bounded by the page. Existing text/tool/message clip limits do not apply; an oversized part refuses without clipping.".to_owned()] })
    }

    fn recording_query(&self, sql: &str, bytes: &mut usize) -> Result<Vec<Value>> {
        let text = self.database_text(sql)?;
        *bytes += text.len();
        strict_database_rows(parse_database_rows_lossy(&text)?)
    }

    fn recording_row(
        &self,
        id: &str,
        past: &str,
        order: &str,
        limits: &Limits,
        filter: bool,
        bytes: &mut usize,
    ) -> Result<Option<Value>> {
        // A raw JSON field is embedded as JSON, avoiding a second layer of
        // string escaping. Leave transport overhead below the fixed 8MiB cap.
        let bound = limits.scan_bytes.min(MAX_COMMAND_BYTES as usize / 2);
        let mut predicates = Vec::new();
        if !limits.kinds.is_empty() && filter {
            if limits.wants(Kind::ToolCall) || limits.wants(Kind::ToolResult) {
                predicates.push("json_extract(p.data, '$.type') = 'tool'");
            }
            if limits.wants(Kind::Reasoning) {
                predicates.push("json_extract(p.data, '$.type') = 'reasoning'");
            }
            if limits.wants(Kind::User) {
                predicates.push("json_extract(p.data, '$.type') = 'text' AND json_extract(m.data, '$.role') = 'user'");
            }
            if limits.wants(Kind::Assistant) {
                predicates.push("json_extract(p.data, '$.type') NOT IN ('tool','reasoning') AND json_extract(m.data, '$.role') = 'assistant'");
            }
            if limits.wants(Kind::Ambient) {
                predicates.push("json_extract(p.data, '$.type') NOT IN ('tool','reasoning') AND json_extract(m.data, '$.role') IN ('system','developer')");
            }
            if limits.wants(Kind::Unknown) {
                predicates.push("1");
            }
        }
        let filter_sql = if limits.kinds.is_empty() || !filter {
            String::new()
        } else if predicates.is_empty() {
            " AND 0".to_owned()
        } else {
            format!(
                " AND ({})",
                predicates
                    .into_iter()
                    .map(|s| format!("({s})"))
                    .collect::<Vec<_>>()
                    .join(" OR ")
            )
        };
        let row = self
            .recording_query(
                &format!(
                    "SELECT json_object('id', p.id, 'time_created', p.time_created, \
             'message_id', p.message_id, 'role', json_extract(m.data, '$.role'), \
             'message_time', json_extract(m.data, '$.time.created'), \
             'bytes', length(CAST(p.data AS BLOB)), 'data', \
             CASE WHEN length(CAST(p.data AS BLOB)) <= {bound} AND json_valid(p.data) \
             THEN json(p.data) END) AS row FROM part p \
             JOIN message m ON m.id = p.message_id AND m.session_id = p.session_id \
             WHERE p.session_id = {id} AND typeof(p.time_created) = 'integer' \
             AND p.time_created >= 0 AND typeof(p.id) = 'text'{past}{filter_sql} \
             ORDER BY p.time_created {order}, p.id {order} LIMIT 1"
                ),
                bytes,
            )?
            .into_iter()
            .next();
        if let Some(row) = &row {
            anyhow::ensure!(row["bytes"].as_u64().unwrap_or(0) <= bound as u64, "OpenCode part exceeds {bound} source bytes; increase scan budget (maximum native part 4MiB), content was not clipped");
        }
        Ok(row)
    }
}
