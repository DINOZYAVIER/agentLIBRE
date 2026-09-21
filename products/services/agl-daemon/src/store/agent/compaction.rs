use std::collections::BTreeMap;

use agl_core::MessageId;
use agl_core::agent::{
    CompactionExactState, FoldedOperationFact, InspectedFileFact, InspectedFileRange,
    OperationFact, PackageDigest,
};
use sha2::{Digest as _, Sha256};

use super::*;

fn invalid(reason: &'static str) -> StoreError {
    StoreError::InvalidValue {
        field: "Compaction",
        value: String::new(),
        reason,
    }
}

fn merge_inspected_file_ranges(entry: &mut InspectedFileFact) {
    entry
        .ranges
        .sort_by_key(|range| (range.start_line, range.end_line));
    let mut merged: Vec<InspectedFileRange> = Vec::new();
    for range in entry.ranges.drain(..) {
        if let Some(last) = merged.last_mut()
            && range.start_line <= last.end_line.saturating_add(1)
        {
            last.end_line = last.end_line.max(range.end_line);
        } else {
            merged.push(range);
        }
    }
    entry.ranges = merged;
}

pub(super) fn conversation_context(
    connection: &rusqlite::Connection,
    conversation_id: ConversationId,
) -> crate::store::Result<Vec<MessageId>> {
    let latest: Option<(String, i64, String)> = connection
        .query_row(
            "SELECT m.message_id, m.message_sequence, o.result_json
         FROM agent_messages m JOIN agent_operations o
           ON o.agent_run_id=m.agent_run_id AND o.ordinal=m.source_operation_ordinal
         WHERE m.conversation_id=?1 AND o.kind='compaction' AND o.state='succeeded'
         ORDER BY m.message_sequence DESC LIMIT 1",
            [conversation_id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let mut context = Vec::new();
    let after = if let Some((message, sequence, result)) = latest {
        let StoredAgentOperationResult::Compaction(metadata) = serde_json::from_str(&result)?
        else {
            return Err(invalid(
                "compaction operation must contain compaction metadata",
            ));
        };
        context.extend(metadata.retained_checkpoints);
        context
            .push(MessageId::parse(&message).map_err(|_| invalid("invalid summary message ID"))?);
        context.extend(metadata.tail);
        sequence
    } else {
        0
    };
    let mut statement = connection.prepare(
        "SELECT message_id FROM agent_messages WHERE conversation_id=?1 AND message_sequence>?2
         ORDER BY message_sequence LIMIT 10000",
    )?;
    let rows = statement.query_map(
        params![conversation_id.as_bytes().as_slice(), after],
        |row| row.get::<_, String>(0),
    )?;
    for row in rows {
        context.push(MessageId::parse(&row?).map_err(|_| invalid("invalid context message ID"))?);
    }
    if context.len() >= 10_000 {
        return Err(invalid(
            "rebuilt context exceeds 10000 messages with the new input",
        ));
    }
    Ok(context)
}

impl StoreHandle {
    pub(crate) fn compaction_exact_state(
        &self,
        key: &AgentOperationKey,
    ) -> crate::store::Result<CompactionExactState> {
        let run = self.agent_run(key.run_id)?;
        let agl_core::agent::AgentRunOrigin::User {
            conversation_id, ..
        } = run.origin;
        let snapshot_digest =
            PackageDigest::from_bytes(Sha256::digest(serde_json::to_vec(&run.snapshot)?).into());
        let operations = self.read(|connection| {
            let mut statement = connection.prepare(
                "SELECT o.agent_run_id, o.ordinal, o.kind, o.tool_id, o.state, o.failure_json,
                     CASE WHEN o.kind='tool' THEN json_extract(o.result_json, '$.result.effect_receipts') END
                 FROM agent_operations o JOIN agent_runs r ON r.agent_run_id=o.agent_run_id
                 WHERE r.origin_conversation_id=?1
                   AND o.state IN ('succeeded','failed','outcome_unknown','cancelled')
                   AND (o.agent_run_id<>?2 OR o.ordinal<?3)
                 ORDER BY r.admitted_at_ms, r.agent_run_id, o.ordinal",
            )?;
            let rows = statement.query_map(params![conversation_id.as_bytes().as_slice(), key.run_id.as_bytes().as_slice(), key.ordinal.get()], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, u32>(1)?, row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?, row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?, row.get::<_, Option<String>>(6)?))
            })?;
            let mut folded = Vec::<FoldedOperationFact>::new();
            let mut indexes = BTreeMap::new();
            for row in rows {
                let (run_id, ordinal, kind, tool_id, status, failure, receipts) = row?;
                let source = AgentOperationKey {
                    run_id: AgentRunId::from_bytes(run_id.try_into().map_err(|_| invalid("invalid fact run ID"))?).map_err(|_| invalid("invalid fact run ID"))?,
                    ordinal: NonZeroU32::new(ordinal).ok_or_else(|| invalid("invalid fact operation ordinal"))?,
                };
                let failure: Option<AgentOperationFailure> = failure.as_deref().map(serde_json::from_str).transpose()?;
                let fact = OperationFact {
                    kind: serde_json::from_value(serde_json::Value::String(kind))?,
                    tool_id: tool_id.map(agl_core::ToolId::new).transpose().map_err(|_| invalid("invalid fact Tool ID"))?,
                    status: serde_json::from_value(serde_json::Value::String(status))?,
                    failure: failure.map(|value| value.kind),
                    effect_receipts: receipts.as_deref().map(serde_json::from_str).transpose()?.unwrap_or_default(),
                };
                let encoded = serde_json::to_string(&fact)?;
                if let Some(index) = indexes.get(&encoded).copied() {
                    let group: &mut FoldedOperationFact = &mut folded[index];
                    group.count += 1;
                    group.last = source.clone();
                    group.sources.push(source);
                } else {
                    indexes.insert(encoded, folded.len());
                    folded.push(FoldedOperationFact { fact, count: 1, first: source.clone(), last: source.clone(), sources: vec![source] });
                }
            }
            Ok(folded)
        })?;
        let inspected_files = self.read(|connection| {
            let mut statement = connection.prepare(
                "SELECT o.request_json, m.message_id, m.content_json
                 FROM agent_operations o
                 JOIN agent_runs r ON r.agent_run_id=o.agent_run_id
                 JOIN agent_messages m ON m.agent_run_id=o.agent_run_id
                   AND m.source_operation_ordinal=o.ordinal
                   AND m.role='tool'
                 WHERE r.origin_conversation_id=?1
                   AND o.tool_id='agentlibre.builtins:fs_read'
                   AND o.state='succeeded'
                   AND (o.agent_run_id<>?2 OR o.ordinal<?3)
                 ORDER BY r.admitted_at_ms, r.agent_run_id, o.ordinal",
            )?;
            let rows = statement.query_map(
                params![
                    conversation_id.as_bytes().as_slice(),
                    key.run_id.as_bytes().as_slice(),
                    key.ordinal.get()
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )?;
            let mut files = BTreeMap::<(String, String), InspectedFileFact>::new();
            for row in rows {
                let (request, message_id, content) = row?;
                let request: serde_json::Value = serde_json::from_str(&request)?;
                let input = request
                    .pointer("/request/input")
                    .and_then(serde_json::Value::as_object);
                let content: serde_json::Value = serde_json::from_str(&content)?;
                let output: serde_json::Value = content
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|text| serde_json::from_str(text).ok())
                    .unwrap_or(content);
                let Some(path) = output
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| {
                        input
                            .and_then(|value| value.get("path"))
                            .and_then(serde_json::Value::as_str)
                    })
                else {
                    continue;
                };
                let Some(digest) = output.get("digest").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let start_line = output
                    .get("start_line")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1);
                let end_line = output
                    .get("end_line")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(start_line);
                let source = MessageId::parse(&message_id)
                    .map_err(|_| invalid("invalid inspected-file source ID"))?;
                let key = (path.to_owned(), digest.to_owned());
                let entry = files.entry(key).or_insert_with(|| InspectedFileFact {
                    path: path.to_owned(),
                    digest: digest.to_owned(),
                    ranges: Vec::new(),
                    sources: Vec::new(),
                });
                entry.ranges.push(InspectedFileRange {
                    start_line,
                    end_line,
                });
                if !entry.sources.contains(&source) {
                    entry.sources.push(source);
                }
            }
            for entry in files.values_mut() {
                merge_inspected_file_ranges(entry);
            }
            Ok(files.into_values().collect())
        })?;
        Ok(CompactionExactState {
            snapshot_run: key.run_id,
            snapshot_digest,
            workspace: run.snapshot.workspace,
            operations,
            inspected_files,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspected_ranges_merge_overlaps_and_adjacent_lines() {
        let mut entry = InspectedFileFact {
            path: "src/lib.rs".into(),
            digest: "sha256:a".into(),
            ranges: vec![
                InspectedFileRange {
                    start_line: 11,
                    end_line: 20,
                },
                InspectedFileRange {
                    start_line: 1,
                    end_line: 10,
                },
                InspectedFileRange {
                    start_line: 19,
                    end_line: 25,
                },
                InspectedFileRange {
                    start_line: 30,
                    end_line: 32,
                },
            ],
            sources: vec![],
        };

        merge_inspected_file_ranges(&mut entry);

        assert_eq!(
            entry.ranges,
            vec![
                InspectedFileRange {
                    start_line: 1,
                    end_line: 25,
                },
                InspectedFileRange {
                    start_line: 30,
                    end_line: 32,
                },
            ]
        );
    }
}
