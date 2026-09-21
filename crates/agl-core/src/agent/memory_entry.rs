//! Memory entry and `INDEX.md` codec (task 3 of `reports/m1-implementation-tasks.md`).
//!
//! Pure string-level parsing, rendering and in-memory mutation of the M1
//! semantic-memory format (`reports/semantic-memory.md` sections 3-5):
//!
//! - one `memory/entries/<slug>.md` per topic, one claim per bullet line;
//! - `INDEX.md` is a projection regenerated from entries, header included in
//!   the 40-line hard cap;
//! - append-only updates: new claims are appended, `updated` is bumped, exact
//!   text is deduplicated per topic, and supersession tags the old claim with
//!   `[superseded-by: <slug>#claim-<n>]` pointing at the new claim's ordinal
//!   without rewriting its text, including cross-topic references (Q1);
//! - new entry titles are derived from the slug (`build-release` ->
//!   `Build release`); INDEX uses the same title and claim count (Q3).
//!
//! This module performs no filesystem I/O: contained, digest-stamped writes
//! belong to task 4, and compaction wiring to task 5. Newly applied claims
//! carry file paths without digests (`FileSource::digest = None`) until the
//! writer stamps `@sha256-12` (M1 D3); existing entries preserve whatever
//! `file:` sources they contain, including file-only claims.

use std::collections::BTreeMap;
use std::fmt;

use crate::agent::memory::{
    MemoryClaimRejection, MemoryTopic, is_valid_slug, is_valid_supersession_ref,
};
use crate::{AgentRunId, MessageId};

/// Hard cap for `INDEX.md`, counting the two header lines (spec section 5).
pub const INDEX_MAX_LINES: usize = 40;

/// The two `INDEX.md` header lines, always present.
pub const INDEX_HEADER: &str = "# Memory Index\n\
    <!-- hard budget: ≤ 40 lines total. One line per entry. Regenerate, never hand-edit semantics. -->\n";

/// One parsed `memory/entries/<slug>.md` file: the canonical claim store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryEntry {
    pub title: String,
    pub slug: String,
    pub created: AgentRunId,
    pub updated: AgentRunId,
    pub artifact: Option<String>,
    pub claims: Vec<StoredClaim>,
}

/// One stored claim bullet. Wording is preserved exactly; claim ordinals are
/// 1-based positions in `MemoryEntry::claims` (`<slug>#claim-<n>`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredClaim {
    pub text: String,
    /// `src:` message IDs; empty for file-only claims, which the seeded
    /// corpus and spec allow.
    pub sources: Vec<MessageId>,
    /// `file:` sources; `digest` is `None` for paths not yet stamped.
    pub files: Vec<FileSource>,
    /// `status:` tag; absent unless the file carries it explicitly.
    pub status: Option<ClaimStatus>,
    /// `superseded-by:` tag pointing at the replacement claim.
    pub superseded_by: Option<String>,
}

/// A `file:` source: workspace-relative path plus optional `sha256-12` digest
/// prefix (first 12 hex characters of the file's `fs_read` digest).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileSource {
    pub path: String,
    pub digest: Option<String>,
}

/// Claim `status:` value. Supersession is not a status (spec 3.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaimStatus {
    Active,
    Stale,
}

/// Outcome of applying one accepted claim (per-claim, like task 1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaimApplyOutcome {
    /// The claim was appended; `ordinal` is its 1-based position.
    Appended { ordinal: usize },
    /// Exact-text duplicate already stored; not rewritten, no rejection.
    Duplicate { ordinal: usize },
    /// The claim was not written; `reason` feeds the rejection recorder.
    Rejected { reason: MemoryClaimRejection },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EntryParseError {
    MissingTitle,
    EmptyTitle,
    DuplicateHeaderLine(usize),
    MissingClaimsSection,
    UnknownField { line: usize, field: String },
    MissingField { field: String },
    InvalidSlug(String),
    InvalidRun { field: String, value: String },
    UnknownStatus { line: usize, value: String },
    InvalidMessageSource { line: usize, value: String },
    InvalidFileSource { line: usize, value: String },
    InvalidSupersededBy { line: usize, value: String },
    DuplicateTag { line: usize, tag: &'static str },
    MalformedClaimLine { line: usize },
    UnsourcefulClaim { line: usize },
    UnexpectedLine { line: usize, text: String },
}

impl fmt::Display for EntryParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTitle => write!(formatter, "entry must start with a '# <title>' line"),
            Self::EmptyTitle => write!(formatter, "entry title must not be empty"),
            Self::DuplicateHeaderLine(line) => {
                write!(
                    formatter,
                    "line {line} is outside the recognized entry layout"
                )
            }
            Self::MissingClaimsSection => {
                write!(formatter, "entry is missing the '## Claims' section")
            }
            Self::UnknownField { line, field } => {
                write!(formatter, "line {line} has unknown entry field {field:?}")
            }
            Self::MissingField { field } => {
                write!(formatter, "entry is missing required field {field:?}")
            }
            Self::InvalidSlug(slug) => write!(formatter, "invalid slug {slug:?}"),
            Self::InvalidRun { field, value } => {
                write!(formatter, "invalid run id for field {field:?}: {value:?}")
            }
            Self::UnknownStatus { line, value } => {
                write!(formatter, "line {line} has unknown status {value:?}")
            }
            Self::InvalidMessageSource { line, value } => {
                write!(
                    formatter,
                    "line {line} has invalid message source {value:?}"
                )
            }
            Self::InvalidFileSource { line, value } => {
                write!(formatter, "line {line} has invalid file source {value:?}")
            }
            Self::InvalidSupersededBy { line, value } => {
                write!(
                    formatter,
                    "line {line} has invalid superseded-by reference {value:?}"
                )
            }
            Self::DuplicateTag { line, tag } => {
                write!(formatter, "line {line} repeats the {tag:?} tag")
            }
            Self::MalformedClaimLine { line } => {
                write!(formatter, "line {line} is not a valid claim bullet")
            }
            Self::UnsourcefulClaim { line } => {
                write!(
                    formatter,
                    "line {line} carries neither a src: nor a file: tag"
                )
            }
            Self::UnexpectedLine { line, text } => {
                write!(formatter, "line {line} is unexpected: {text:?}")
            }
        }
    }
}

impl std::error::Error for EntryParseError {}

/// Corpus-level parse failure: one bad entry file fails the whole corpus.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CorpusParseError {
    pub path: String,
    pub source: EntryParseError,
}

impl fmt::Display for CorpusParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "entry file {} is invalid: {}",
            self.path, self.source
        )
    }
}

impl std::error::Error for CorpusParseError {}

/// Parse one entry file (spec 3.1). Exact wording of claim text and all tags
/// is preserved; `render_entry` reproduces the parsed entry byte for byte.
pub fn parse_entry(text: &str) -> Result<MemoryEntry, EntryParseError> {
    let mut lines = text.lines().enumerate();
    let Some((0, first)) = lines.next() else {
        return Err(EntryParseError::MissingTitle);
    };
    let Some(title) = first.strip_prefix("# ") else {
        return Err(EntryParseError::MissingTitle);
    };
    if title.is_empty() {
        return Err(EntryParseError::EmptyTitle);
    }

    let mut slug: Option<String> = None;
    let mut created: Option<AgentRunId> = None;
    let mut updated: Option<AgentRunId> = None;
    let mut artifact: Option<String> = None;
    let mut claims: Vec<StoredClaim> = Vec::new();
    let mut in_claims = false;

    for (index, line) in lines {
        let line_no = index + 1;
        if line.is_empty() {
            continue;
        }
        if !in_claims {
            if line == "## Claims" {
                in_claims = true;
                continue;
            }
            let Some((field, value)) = line
                .strip_prefix("- ")
                .and_then(|rest| rest.split_once(": "))
            else {
                return Err(EntryParseError::UnexpectedLine {
                    line: line_no,
                    text: line.to_owned(),
                });
            };
            match field {
                "slug" => {
                    if slug.is_some() {
                        return Err(EntryParseError::DuplicateHeaderLine(line_no));
                    }
                    slug = Some(value.to_owned());
                }
                "created" => {
                    if created.is_some() {
                        return Err(EntryParseError::DuplicateHeaderLine(line_no));
                    }
                    let value = value.to_owned();
                    created = Some(AgentRunId::parse(&value).map_err(|_| {
                        EntryParseError::InvalidRun {
                            field: "created".to_owned(),
                            value,
                        }
                    })?);
                }
                "updated" => {
                    if updated.is_some() {
                        return Err(EntryParseError::DuplicateHeaderLine(line_no));
                    }
                    let value = value.to_owned();
                    updated = Some(AgentRunId::parse(&value).map_err(|_| {
                        EntryParseError::InvalidRun {
                            field: "updated".to_owned(),
                            value,
                        }
                    })?);
                }
                "artifact" => {
                    if artifact.is_some() {
                        return Err(EntryParseError::DuplicateHeaderLine(line_no));
                    }
                    artifact = Some(value.to_owned());
                }
                _ => {
                    return Err(EntryParseError::UnknownField {
                        line: line_no,
                        field: field.to_owned(),
                    });
                }
            }
            continue;
        }

        let claim = parse_claim_line(line, line_no)?;
        claims.push(claim);
    }

    if !in_claims {
        return Err(EntryParseError::MissingClaimsSection);
    }
    let Some(slug) = slug else {
        return Err(EntryParseError::MissingField {
            field: "slug".to_owned(),
        });
    };
    if !is_valid_slug(&slug) {
        return Err(EntryParseError::InvalidSlug(slug));
    }
    let created = created.ok_or_else(|| EntryParseError::MissingField {
        field: "created".to_owned(),
    })?;
    let updated = updated.ok_or_else(|| EntryParseError::MissingField {
        field: "updated".to_owned(),
    })?;

    Ok(MemoryEntry {
        title: title.to_owned(),
        slug,
        created,
        updated,
        artifact,
        claims,
    })
}

/// Render an entry (spec 3.1) in canonical tag order: `src`, `file`,
/// `status`, `superseded-by`. Claims without message sources omit the `src`
/// tag; paths without digests render without `@<digest>`.
pub fn render_entry(entry: &MemoryEntry) -> String {
    let mut out = String::new();
    out.push_str("# ");
    out.push_str(&entry.title);
    out.push_str("\n\n- slug: ");
    out.push_str(&entry.slug);
    out.push_str("\n- created: ");
    out.push_str(&entry.created.to_string());
    out.push_str("\n- updated: ");
    out.push_str(&entry.updated.to_string());
    if let Some(artifact) = &entry.artifact {
        out.push_str("\n- artifact: ");
        out.push_str(artifact);
    }
    out.push('\n');
    out.push('\n');
    out.push_str("## Claims\n");
    if !entry.claims.is_empty() {
        out.push('\n');
        for claim in &entry.claims {
            out.push_str(&render_claim_line(claim));
            out.push('\n');
        }
    }
    out
}

/// Derive a new entry's title from its slug: `build-release` becomes
/// `Build release` (Q3). Existing entries keep their stored title.
pub fn title_from_slug(slug: &str) -> String {
    let mut segments = slug.split('-');
    let mut title = String::new();
    if let Some(first) = segments.next() {
        let mut chars = first.chars();
        if let Some(character) = chars.next() {
            title.extend(character.to_uppercase());
            title.push_str(chars.as_str());
        }
    }
    for segment in segments {
        title.push(' ');
        title.push_str(segment);
    }
    title
}

/// Parse one `- <text> [tags]` bullet. Tags are read right to left; when a
/// trailing bracket group is not a known tag, tag parsing stops and the group
/// is preserved as claim text, so text containing bracket groups round-trips
/// verbatim. A `]` with no opening `[` stays malformed.
fn parse_claim_line(line: &str, line_no: usize) -> Result<StoredClaim, EntryParseError> {
    let Some(rest) = line.strip_prefix("- ") else {
        return Err(EntryParseError::MalformedClaimLine { line: line_no });
    };
    let mut body = rest;
    let mut sources: Vec<MessageId> = Vec::new();
    let mut files: Vec<FileSource> = Vec::new();
    let mut status: Option<ClaimStatus> = None;
    let mut superseded_by: Option<String> = None;

    while body.ends_with(']') {
        let Some(start) = find_opening_bracket(body) else {
            return Err(EntryParseError::MalformedClaimLine { line: line_no });
        };
        let content = &body[start + 1..body.len() - 1];
        let Some((key, value)) = content.split_once(": ") else {
            // Not a tag: keep the bracket group as claim text.
            break;
        };
        match key {
            "src" => {
                if !sources.is_empty() {
                    return Err(EntryParseError::DuplicateTag {
                        line: line_no,
                        tag: "src",
                    });
                }
                sources = parse_source_list(value, line_no)?;
            }
            "file" => {
                if !files.is_empty() {
                    return Err(EntryParseError::DuplicateTag {
                        line: line_no,
                        tag: "file",
                    });
                }
                files = parse_file_list(value, line_no)?;
            }
            "status" => {
                if status.is_some() {
                    return Err(EntryParseError::DuplicateTag {
                        line: line_no,
                        tag: "status",
                    });
                }
                status = Some(match value {
                    "active" => ClaimStatus::Active,
                    "stale" => ClaimStatus::Stale,
                    _ => {
                        return Err(EntryParseError::UnknownStatus {
                            line: line_no,
                            value: value.to_owned(),
                        });
                    }
                });
            }
            "superseded-by" => {
                if superseded_by.is_some() {
                    return Err(EntryParseError::DuplicateTag {
                        line: line_no,
                        tag: "superseded-by",
                    });
                }
                if !is_valid_supersession_ref(value) {
                    return Err(EntryParseError::InvalidSupersededBy {
                        line: line_no,
                        value: value.to_owned(),
                    });
                }
                superseded_by = Some(value.to_owned());
            }
            _ => {
                // Unknown tag: keep the bracket group as claim text.
                break;
            }
        }
        body = body[..start].trim_end();
    }

    let text = body.trim();
    if text.is_empty() {
        return Err(EntryParseError::MalformedClaimLine { line: line_no });
    }
    if sources.is_empty() && files.is_empty() {
        return Err(EntryParseError::UnsourcefulClaim { line: line_no });
    }

    Ok(StoredClaim {
        text: text.to_owned(),
        sources,
        files,
        status,
        superseded_by,
    })
}

/// Index of the `[` that opens the bracket group ending at `body`'s last `]`.
fn find_opening_bracket(body: &str) -> Option<usize> {
    body.rfind('[')
}

fn parse_source_list(value: &str, line_no: usize) -> Result<Vec<MessageId>, EntryParseError> {
    let mut sources = Vec::new();
    for item in value.split(',') {
        let id = item.trim();
        if id.is_empty() {
            return Err(EntryParseError::MalformedClaimLine { line: line_no });
        }
        sources.push(
            MessageId::parse(id).map_err(|_| EntryParseError::InvalidMessageSource {
                line: line_no,
                value: id.to_owned(),
            })?,
        );
    }
    if sources.is_empty() {
        return Err(EntryParseError::MalformedClaimLine { line: line_no });
    }
    Ok(sources)
}

fn parse_file_list(value: &str, line_no: usize) -> Result<Vec<FileSource>, EntryParseError> {
    let mut files = Vec::new();
    for item in value.split(',') {
        let item = item.trim();
        if item.is_empty() {
            return Err(EntryParseError::MalformedClaimLine { line: line_no });
        }
        let (path, digest) = match item.rsplit_once('@') {
            Some((path, digest)) => {
                if !is_digest12(digest) {
                    return Err(EntryParseError::InvalidFileSource {
                        line: line_no,
                        value: item.to_owned(),
                    });
                }
                (path, Some(digest.to_owned()))
            }
            None => (item, None),
        };
        if path.is_empty() || path.starts_with('/') || path.contains('\0') {
            return Err(EntryParseError::InvalidFileSource {
                line: line_no,
                value: item.to_owned(),
            });
        }
        files.push(FileSource {
            path: path.to_owned(),
            digest,
        });
    }
    if files.is_empty() {
        return Err(EntryParseError::MalformedClaimLine { line: line_no });
    }
    Ok(files)
}

fn is_digest12(digest: &str) -> bool {
    digest.len() == 12 && digest.bytes().all(|b| b.is_ascii_hexdigit())
}

fn render_claim_line(claim: &StoredClaim) -> String {
    let mut line = String::from("- ");
    line.push_str(&claim.text);
    if !claim.sources.is_empty() {
        line.push_str(" [src: ");
        let joined: Vec<String> = claim.sources.iter().map(|id| id.to_string()).collect();
        line.push_str(&joined.join(", "));
        line.push(']');
    }
    if !claim.files.is_empty() {
        line.push_str(" [file: ");
        let joined: Vec<String> = claim
            .files
            .iter()
            .map(|file| match &file.digest {
                Some(digest) => format!("{}@{}", file.path, digest),
                None => file.path.clone(),
            })
            .collect();
        line.push_str(&joined.join(", "));
        line.push(']');
    }
    if let Some(status) = claim.status {
        line.push_str(match status {
            ClaimStatus::Active => " [status: active]",
            ClaimStatus::Stale => " [status: stale]",
        });
    }
    if let Some(reference) = &claim.superseded_by {
        line.push_str(" [superseded-by: ");
        line.push_str(reference);
        line.push(']');
    }
    line
}

/// The parsed `memory/` entry corpus: entries are the source of truth, keyed
/// by slug and stored in slug order for a deterministic index.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MemoryCorpus {
    entries: BTreeMap<String, MemoryEntry>,
}

impl MemoryCorpus {
    /// Parse a set of entry files. Keys are entry paths (`entries/<slug>.md`
    /// or `<slug>.md`); the file slug must match the entry's `slug:` field.
    pub fn parse(files: &BTreeMap<String, String>) -> Result<Self, CorpusParseError> {
        let mut entries = BTreeMap::new();
        for (path, content) in files {
            let file_slug = path
                .rsplit('/')
                .next()
                .and_then(|name| name.strip_suffix(".md"))
                .unwrap_or("");
            let entry = parse_entry(content).map_err(|source| CorpusParseError {
                path: path.clone(),
                source,
            })?;
            if entry.slug != file_slug {
                return Err(CorpusParseError {
                    path: path.clone(),
                    source: EntryParseError::InvalidSlug(entry.slug.clone()),
                });
            }
            entries.insert(entry.slug.clone(), entry);
        }
        Ok(Self { entries })
    }

    /// Insert or replace an entry; the corpus keeps one entry per slug.
    pub fn insert(&mut self, entry: MemoryEntry) {
        self.entries.insert(entry.slug.clone(), entry);
    }

    pub fn entry(&self, slug: &str) -> Option<&MemoryEntry> {
        self.entries.get(slug)
    }

    pub fn entry_mut(&mut self, slug: &str) -> Option<&mut MemoryEntry> {
        self.entries.get_mut(slug)
    }

    pub fn slugs(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Line count of the rendered index, counting its header (spec 5).
    pub fn index_lines(&self) -> usize {
        2 + self.entries.len()
    }

    /// A new topic fits under the cap only when the rendered index with the
    /// added line stays within `INDEX_MAX_LINES`.
    pub fn fits_new_topic(&self) -> bool {
        2 + self.entries.len() < INDEX_MAX_LINES
    }

    /// Regenerate `INDEX.md` from the entries (spec 3.2/5): one line per
    /// entry, slug, entry title (derived from the slug for new topics, Q3),
    /// claim count and path. The header is always present.
    pub fn render_index(&self) -> String {
        let mut out = String::from(INDEX_HEADER);
        for (slug, entry) in &self.entries {
            out.push_str(&format!(
                "- {} — {} ({} claims) — entries/{}.md\n",
                slug,
                entry.title,
                entry.claims.len(),
                slug
            ));
        }
        out
    }

    /// Apply the accepted claims of one topic, in claim order (spec 4):
    ///
    /// - exact-text duplicates are detected and emit no change (no rejection);
    /// - a new topic is refused with `IndexCapReached` when it would push the
    ///   header-inclusive index past `INDEX_MAX_LINES`;
    /// - invalid `supersedes` references (unknown target topic, out-of-range
    ///   ordinal, or a same-topic claim not yet stored) reject the new claim
    ///   without touching any entry; a valid one tags the old claim with
    ///   `[superseded-by: <slug>#claim-<n>]` pointing at the new claim's
    ///   ordinal, preserving the old wording (Q1); a cross-topic reference
    ///   tags the target entry's claim in place, without rewriting its
    ///   wording, and bumps the target's `updated` run to the current run;
    /// - appending bumps `updated` to the current run; a brand-new entry
    ///   takes `created == updated` and the slug-derived title (Q3).
    ///
    /// Newly stored claims carry file paths without digests; the writer
    /// (task 4) stamps `@sha256-12` at write time (M1 D3).
    pub fn apply_topic(
        &mut self,
        topic: &MemoryTopic,
        current_run: &AgentRunId,
    ) -> Vec<ClaimApplyOutcome> {
        if !is_valid_slug(&topic.slug) {
            return topic
                .claims
                .iter()
                .map(|_| ClaimApplyOutcome::Rejected {
                    reason: MemoryClaimRejection::InvalidSlug,
                })
                .collect();
        }
        let existing = self.entries.contains_key(&topic.slug);
        if !existing && !self.fits_new_topic() {
            return topic
                .claims
                .iter()
                .map(|_| ClaimApplyOutcome::Rejected {
                    reason: MemoryClaimRejection::IndexCapReached,
                })
                .collect();
        }

        let mut entry = self
            .entries
            .remove(&topic.slug)
            .unwrap_or_else(|| MemoryEntry {
                title: title_from_slug(&topic.slug),
                slug: topic.slug.clone(),
                created: *current_run,
                updated: *current_run,
                artifact: None,
                claims: Vec::new(),
            });

        let mut outcomes = Vec::with_capacity(topic.claims.len());
        for claim in &topic.claims {
            if let Some(position) = entry
                .claims
                .iter()
                .position(|stored| stored.text == claim.text)
            {
                outcomes.push(ClaimApplyOutcome::Duplicate {
                    ordinal: position + 1,
                });
                continue;
            }
            if claim.sources.is_empty() && claim.files.is_empty() {
                outcomes.push(ClaimApplyOutcome::Rejected {
                    reason: MemoryClaimRejection::MissingSources,
                });
                continue;
            }
            // Same-topic references must point at a claim already stored;
            // cross-topic references must point into an existing entry.
            let mut supersession_valid = true;
            let mut local: Vec<usize> = Vec::new();
            let mut cross: Vec<(String, usize)> = Vec::new();
            for reference in claim.supersedes.iter().flatten() {
                let Some((ref_slug, ordinal)) = reference.split_once('#') else {
                    supersession_valid = false;
                    break;
                };
                let Some(number) = ordinal.strip_prefix("claim-") else {
                    supersession_valid = false;
                    break;
                };
                let Some(position) = number
                    .parse::<usize>()
                    .ok()
                    .and_then(|value| value.checked_sub(1))
                else {
                    supersession_valid = false;
                    break;
                };
                if ref_slug == topic.slug {
                    if position >= entry.claims.len() {
                        supersession_valid = false;
                        break;
                    }
                    local.push(position);
                } else if let Some(target) = self.entries.get(ref_slug) {
                    if position >= target.claims.len() {
                        supersession_valid = false;
                        break;
                    }
                    cross.push((ref_slug.to_owned(), position));
                } else {
                    supersession_valid = false;
                    break;
                }
            }
            if !supersession_valid {
                outcomes.push(ClaimApplyOutcome::Rejected {
                    reason: MemoryClaimRejection::InvalidSupersession,
                });
                continue;
            }

            let ordinal = entry.claims.len() + 1;
            let stored = StoredClaim {
                text: claim.text.clone(),
                sources: claim.sources.clone(),
                files: claim
                    .files
                    .iter()
                    .map(|path| FileSource {
                        path: path.clone(),
                        digest: None,
                    })
                    .collect(),
                status: None,
                superseded_by: None,
            };
            for position in &local {
                if entry.claims[*position].superseded_by.is_none() {
                    entry.claims[*position].superseded_by =
                        Some(format!("{}#claim-{}", topic.slug, ordinal));
                }
            }
            for (ref_slug, position) in &cross {
                if let Some(target) = self
                    .entries
                    .get_mut(ref_slug)
                    .filter(|target| target.claims[*position].superseded_by.is_none())
                {
                    target.claims[*position].superseded_by =
                        Some(format!("{}#claim-{}", topic.slug, ordinal));
                    target.updated = *current_run;
                }
            }
            entry.claims.push(stored);
            entry.updated = *current_run;
            outcomes.push(ClaimApplyOutcome::Appended { ordinal });
        }

        let appended = outcomes
            .iter()
            .any(|outcome| matches!(outcome, ClaimApplyOutcome::Appended { .. }));
        // A brand-new topic is materialized only when at least one claim is
        // appended; a topic whose claims are all rejected stays absent.
        if existing || appended {
            self.entries.insert(topic.slug.clone(), entry);
        }
        outcomes
    }
}

/// One parsed `INDEX.md` body line: slug, entry title, claim count and entry
/// path exactly as `MemoryCorpus::render_index` emits them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexLine {
    pub slug: String,
    pub title: String,
    pub claims: usize,
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndexParseError {
    /// The first two lines are not the required `INDEX_HEADER`.
    MissingHeader,
    /// A body line is not a `- <slug> — <title> (<n> claims) — <path>` bullet.
    MalformedLine { line: usize, text: String },
    /// The bullet's slug is not a valid kebab-case slug.
    InvalidSlug { line: usize, slug: String },
    /// The claim count is not a base-10 number.
    InvalidClaimCount { line: usize, value: String },
    /// The entry path is not `entries/<slug>.md` for the bullet's slug.
    PathMismatch {
        line: usize,
        slug: String,
        path: String,
    },
}

impl fmt::Display for IndexParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHeader => write!(formatter, "INDEX.md is missing the two-line header"),
            Self::MalformedLine { line, text } => {
                write!(
                    formatter,
                    "INDEX.md line {line} is not a valid entry bullet: {text:?}"
                )
            }
            Self::InvalidSlug { line, slug } => {
                write!(formatter, "INDEX.md line {line} has invalid slug {slug:?}")
            }
            Self::InvalidClaimCount { line, value } => {
                write!(
                    formatter,
                    "INDEX.md line {line} has invalid claim count {value:?}"
                )
            }
            Self::PathMismatch { line, slug, path } => {
                write!(
                    formatter,
                    "INDEX.md line {line} path {path:?} does not match slug {slug:?}"
                )
            }
        }
    }
}

impl std::error::Error for IndexParseError {}

/// Render one index body line exactly as `MemoryCorpus::render_index` emits it.
pub fn render_index_line(line: &IndexLine) -> String {
    format!(
        "- {} — {} ({} claims) — {}\n",
        line.slug, line.title, line.claims, line.path
    )
}

/// Parse `INDEX.md` (spec 3.2). The two `INDEX_HEADER` lines are required;
/// every further line is one `- <slug> — <title> (<n> claims) —
/// entries/<slug>.md` bullet. `parse_index(corpus.render_index())` recovers
/// the index lines, and rendering them back reproduces the text.
pub fn parse_index(text: &str) -> Result<Vec<IndexLine>, IndexParseError> {
    let Some(rest) = text.strip_prefix(INDEX_HEADER) else {
        return Err(IndexParseError::MissingHeader);
    };
    let mut lines = Vec::new();
    for (offset, line) in rest.lines().enumerate() {
        // Line numbers count the two header lines first.
        lines.push(parse_index_line(line, offset + 3)?);
    }
    Ok(lines)
}

fn parse_index_line(line: &str, line_no: usize) -> Result<IndexLine, IndexParseError> {
    let malformed = || IndexParseError::MalformedLine {
        line: line_no,
        text: line.to_owned(),
    };
    let Some(rest) = line.strip_prefix("- ") else {
        return Err(malformed());
    };
    let (head, path) = rest.rsplit_once(" — entries/").ok_or_else(malformed)?;
    let path = format!("entries/{path}");
    if !path.ends_with(".md") {
        return Err(malformed());
    }
    let (slug, tail) = head.split_once(" — ").ok_or_else(malformed)?;
    if !is_valid_slug(slug) {
        return Err(IndexParseError::InvalidSlug {
            line: line_no,
            slug: slug.to_owned(),
        });
    }
    if path != format!("entries/{slug}.md") {
        return Err(IndexParseError::PathMismatch {
            line: line_no,
            slug: slug.to_owned(),
            path,
        });
    }
    let (title, count) = tail.rsplit_once(" (").ok_or_else(malformed)?;
    let Some(number) = count.strip_suffix(" claims)") else {
        return Err(malformed());
    };
    let claims = number
        .parse::<usize>()
        .map_err(|_| IndexParseError::InvalidClaimCount {
            line: line_no,
            value: number.to_owned(),
        })?;
    if title.is_empty() {
        return Err(malformed());
    }
    Ok(IndexLine {
        slug: slug.to_owned(),
        title: title.to_owned(),
        claims,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::memory::MemoryClaim;

    const RUN_A: &str = "run_01a090d4-5628-77d3-8f53-1abe34dbda5d";
    const RUN_B: &str = "run_01a09292-e4ab-7e11-a6ee-1221312293c1";
    const RUN_C: &str = "run_01a09500-abcd-7000-8000-000000000000";
    const MSG_A: &str = "msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9";
    const MSG_B: &str = "msg_01a090d5-4b9b-71b0-aae8-d5421f5c3513";

    /// Verbatim fixture: the seeded `memory/entries/repl.md` corpus entry.
    const SEEDED_REPL: &str = r"# REPL architecture

- slug: repl
- created: run_01a090d4-5628-77d3-8f53-1abe34dbda5d
- updated: run_01a09292-e4ab-7e11-a6ee-1221312293c1
- artifact: reports/repl-architecture.md

## Claims

- The REPL is the default (no-subcommand) mode of the `agl` CLI. [src: msg_01a090d5-03b2-7a01-ac4c-8c7b192c76e9, msg_01a090d5-4b9b-71b0-aae8-d5421f5c3513] [file: products/agl-cli/src/lib.rs@778b0789cbd7]
- Input uses reedline (Emacs keybindings) with SlashCompleter, ListMenu (Tab or /), DockHinter. [src: msg_01a090d5-e0fe-76e2-ab23-107cd6e69dd0] [file: products/agl-cli/src/lib.rs@778b0789cbd7]
- Daemon protocol: Unix socket, JSONL, schema `agentlibre.agent.v1alpha`, 8 MiB max frame. [src: msg_01a090d8-9e45-7923-8e85-0a14c31a29ec, msg_01a090d8-b3e5-75a1-89d9-26cc3ac272d4] [file: crates/agl-daemon-api/src/agent.rs@b9e79fa893fe, crates/agl-daemon-api/src/lib.rs@8ea676989617]
- Model generation is measured before the call; prompt ≥ 70% of capacity (ceil(7/10·cap)) fails with CompactionRequired. [file: crates/agl-core/src/agent/context.rs@3a4aa91013fd, products/services/agl-daemon/src/agent/operation_driver.rs@87d4b4ce57ad]
- Session lifecycle: the CLI connects to the daemon, opens (or resumes) a conversation; each submitted prompt starts a run via `AgentClient::start_run`; the REPL renders the run's `AgentEvent` stream live and prints a `RUN` summary with elapsed time on terminal status. [file: reports/repl-architecture.md@332f51b1a48c, crates/agl-daemon-api/src/client.rs@ef5d43ad6051]
- `AgentProgress` (`ModelOutputDelta`, `OperationStatus`) is process-local and never persisted; durable consumers resync via `AgentRunView` and `AgentEventPage`. [file: reports/repl-daemon-protocol.md@5d13c5f1ba2d, crates/agl-daemon-api/src/progress.rs@d2013283ecc0]
- Operation requests are typed JSON objects on the wire; `AgentOperationKind` has exactly three variants: `ModelGeneration`, `Compaction`, `Tool`. [file: reports/repl-daemon-protocol.md@5d13c5f1ba2d, crates/agl-core/src/agent/operation.rs@38b1858893c7]
- `AgentClient` (crate `agl-daemon-api`) exposes `open_conversation`, `start_run`, `subscribe`, `rename_conversation`. [file: reports/repl-daemon-protocol.md@5d13c5f1ba2d, crates/agl-daemon-api/src/client.rs@ef5d43ad6051]
- Prompt history is persisted as JSON lines to `<state>/repl/history.reedline` (XDG state root `agentLIBRE`), capped at 1000 entries with the oldest dropped. [file: reports/repl-terminal-ui.md@0dfba85063f6, products/agl-cli/src/lib.rs@778b0789cbd7]
- `TtyRenderer` owns the terminal display state for the session (theme, activity string, streaming buffer, transcript newline state); model markdown is rendered with ANSI colors. [file: reports/repl-terminal-ui.md@0dfba85063f6, products/agl-cli/src/lib.rs@778b0789cbd7]
";

    fn run(value: &str) -> AgentRunId {
        AgentRunId::parse(value).unwrap()
    }

    fn msg(value: &str) -> MessageId {
        MessageId::parse(value).unwrap()
    }

    fn new_claim(text: &str, sources: Vec<MessageId>, files: Vec<String>) -> MemoryClaim {
        MemoryClaim {
            text: text.to_owned(),
            sources,
            files,
            supersedes: None,
        }
    }

    fn superseding_claim(text: &str, sources: Vec<MessageId>, refs: Vec<&str>) -> MemoryClaim {
        MemoryClaim {
            text: text.to_owned(),
            sources,
            files: vec![],
            supersedes: Some(refs.into_iter().map(str::to_owned).collect()),
        }
    }

    fn topic(slug: &str, claims: Vec<MemoryClaim>) -> MemoryTopic {
        MemoryTopic {
            slug: slug.to_owned(),
            claims,
        }
    }

    fn two_claim_entry() -> MemoryEntry {
        MemoryEntry {
            title: "REPL architecture".to_owned(),
            slug: "repl".to_owned(),
            created: run(RUN_A),
            updated: run(RUN_B),
            artifact: Some("reports/repl-architecture.md".to_owned()),
            claims: vec![
                StoredClaim {
                    text: "Old first fact.".to_owned(),
                    sources: vec![msg(MSG_A)],
                    files: vec![],
                    status: None,
                    superseded_by: None,
                },
                StoredClaim {
                    text: "Old second fact.".to_owned(),
                    sources: vec![],
                    files: vec![FileSource {
                        path: "crates/agl-core/src/lib.rs".to_owned(),
                        digest: Some("3a4aa91013fd".to_owned()),
                    }],
                    status: None,
                    superseded_by: None,
                },
            ],
        }
    }

    fn single_claim_entry(slug: &str, title: &str, text: &str) -> MemoryEntry {
        MemoryEntry {
            title: title.to_owned(),
            slug: slug.to_owned(),
            created: run(RUN_A),
            updated: run(RUN_A),
            artifact: None,
            claims: vec![StoredClaim {
                text: text.to_owned(),
                sources: vec![msg(MSG_A)],
                files: vec![],
                status: None,
                superseded_by: None,
            }],
        }
    }

    #[test]
    fn claim_text_with_bracket_groups_is_preserved_verbatim() {
        let text = format!(
            "# Demo\n\n- slug: demo\n- created: {RUN_A}\n- updated: {RUN_B}\n\n## Claims\n\n\
             - The list uses [brackets] and [a: b] groups. [src: {MSG_A}]\n\
             - The wording keeps [ref: old] [src: {MSG_B}] [file: notes/demo.md@3a4aa91013fd]\n"
        );
        let entry = parse_entry(&text).unwrap();
        assert_eq!(
            entry.claims[0].text,
            "The list uses [brackets] and [a: b] groups."
        );
        assert_eq!(entry.claims[0].sources, vec![msg(MSG_A)]);
        assert!(entry.claims[0].files.is_empty());
        assert_eq!(entry.claims[1].text, "The wording keeps [ref: old]");
        assert_eq!(entry.claims[1].sources, vec![msg(MSG_B)]);
        assert_eq!(
            entry.claims[1].files,
            vec![FileSource {
                path: "notes/demo.md".to_owned(),
                digest: Some("3a4aa91013fd".to_owned()),
            }]
        );
        assert_eq!(render_entry(&entry), text);
    }

    #[test]
    fn index_round_trips_through_parse_and_render() {
        let mut corpus = MemoryCorpus::default();
        corpus.insert(parse_entry(SEEDED_REPL).unwrap());
        corpus.insert(single_claim_entry(
            "build-release",
            "Build release",
            "The release train ships on Fridays.",
        ));
        let rendered = corpus.render_index();
        let lines = parse_index(&rendered).unwrap();
        assert_eq!(
            lines,
            vec![
                IndexLine {
                    slug: "build-release".to_owned(),
                    title: "Build release".to_owned(),
                    claims: 1,
                    path: "entries/build-release.md".to_owned(),
                },
                IndexLine {
                    slug: "repl".to_owned(),
                    title: "REPL architecture".to_owned(),
                    claims: 10,
                    path: "entries/repl.md".to_owned(),
                },
            ]
        );
        let reconstructed =
            INDEX_HEADER.to_owned() + &lines.iter().map(render_index_line).collect::<String>();
        assert_eq!(reconstructed, rendered);

        // An empty corpus renders just the header and parses to zero lines.
        let empty = MemoryCorpus::default();
        assert_eq!(
            parse_index(&empty.render_index()).unwrap(),
            Vec::<IndexLine>::new()
        );
    }

    #[test]
    fn index_parse_rejects_malformed_lines() {
        assert!(matches!(
            parse_index(""),
            Err(IndexParseError::MissingHeader)
        ));
        assert!(matches!(
            parse_index("# Memory Index\n"),
            Err(IndexParseError::MissingHeader)
        ));

        let good =
            format!("{INDEX_HEADER}- repl — REPL architecture (10 claims) — entries/repl.md\n");
        assert_eq!(parse_index(&good).unwrap().len(), 1);

        let not_bullet = format!("{INDEX_HEADER}plain note\n");
        assert!(matches!(
            parse_index(&not_bullet),
            Err(IndexParseError::MalformedLine { line: 3, .. })
        ));

        let bad_count =
            format!("{INDEX_HEADER}- repl — REPL architecture (ten claims) — entries/repl.md\n");
        assert!(matches!(
            parse_index(&bad_count),
            Err(IndexParseError::InvalidClaimCount { line: 3, .. })
        ));

        let path_mismatch =
            format!("{INDEX_HEADER}- repl — REPL architecture (10 claims) — entries/other.md\n");
        assert!(matches!(
            parse_index(&path_mismatch),
            Err(IndexParseError::PathMismatch { line: 3, .. })
        ));

        let bad_slug =
            format!("{INDEX_HEADER}- Repl — REPL architecture (10 claims) — entries/Repl.md\n");
        assert!(matches!(
            parse_index(&bad_slug),
            Err(IndexParseError::InvalidSlug { line: 3, .. })
        ));
    }

    #[test]
    fn cross_topic_supersession_tags_the_other_entry() {
        let mut corpus = MemoryCorpus::default();
        corpus.insert(two_claim_entry());
        corpus.insert(single_claim_entry(
            "daemon",
            "Daemon notes",
            "The daemon binds one socket.",
        ));

        let outcomes = corpus.apply_topic(
            &topic(
                "repl",
                vec![superseding_claim(
                    "The REPL runs in-process now.",
                    vec![msg(MSG_B)],
                    vec!["daemon#claim-1"],
                )],
            ),
            &run(RUN_C),
        );

        assert_eq!(outcomes, vec![ClaimApplyOutcome::Appended { ordinal: 3 }]);
        let daemon = corpus.entry("daemon").unwrap();
        assert_eq!(
            daemon.claims[0].superseded_by.as_deref(),
            Some("repl#claim-3")
        );
        // The target's wording is preserved; only the tag is added.
        assert_eq!(daemon.claims[0].text, "The daemon binds one socket.");
        // Tagging the target counts as an edit: its `updated` run moves to
        // the current run even though no claim was appended to it.
        assert_eq!(
            daemon.updated,
            run(RUN_C),
            "cross-topic tagging must bump the target's updated run"
        );
        let rendered = render_entry(daemon);
        assert!(rendered.contains(&format!(
            "- The daemon binds one socket. [src: {MSG_A}] [superseded-by: repl#claim-3]\n"
        )));
        assert_eq!(parse_entry(&rendered).unwrap(), daemon.clone());

        let repl = corpus.entry("repl").unwrap();
        assert_eq!(repl.claims[2].text, "The REPL runs in-process now.");
        assert_eq!(repl.claims[0].superseded_by, None);
    }

    #[test]
    fn cross_topic_invalid_supersession_rejects_and_leaves_target_untouched() {
        for reference in ["ghost#claim-1", "daemon#claim-2", "daemon#claim-0"] {
            let mut corpus = MemoryCorpus::default();
            corpus.insert(single_claim_entry(
                "daemon",
                "Daemon notes",
                "The daemon binds one socket.",
            ));
            let before = render_entry(corpus.entry("daemon").unwrap());

            let outcomes = corpus.apply_topic(
                &topic(
                    "repl",
                    vec![superseding_claim(
                        "The REPL runs in-process now.",
                        vec![msg(MSG_B)],
                        vec![reference],
                    )],
                ),
                &run(RUN_C),
            );

            assert_eq!(
                outcomes,
                vec![ClaimApplyOutcome::Rejected {
                    reason: MemoryClaimRejection::InvalidSupersession
                }],
                "reference {reference:?}"
            );
            assert_eq!(render_entry(corpus.entry("daemon").unwrap()), before);
            assert!(
                corpus.entry("repl").is_none(),
                "a rejected cross-topic claim must not create the topic"
            );
        }
    }

    #[test]
    fn seeded_entry_round_trips_verbatim() {
        let entry = parse_entry(SEEDED_REPL).unwrap();
        assert_eq!(entry.title, "REPL architecture");
        assert_eq!(entry.slug, "repl");
        assert_eq!(entry.created, run(RUN_A));
        assert_eq!(entry.updated, run(RUN_B));
        assert_eq!(
            entry.artifact.as_deref(),
            Some("reports/repl-architecture.md")
        );
        assert_eq!(entry.claims.len(), 10);

        let first = &entry.claims[0];
        assert_eq!(
            first.text,
            "The REPL is the default (no-subcommand) mode of the `agl` CLI."
        );
        assert_eq!(first.sources, vec![msg(MSG_A), msg(MSG_B)]);
        assert_eq!(
            first.files,
            vec![FileSource {
                path: "products/agl-cli/src/lib.rs".to_owned(),
                digest: Some("778b0789cbd7".to_owned()),
            }]
        );

        // Claim 3 (ordinal 3) carries two `file:` sources in one tag.
        assert_eq!(entry.claims[2].files.len(), 2);
        // Claim 4 is file-only: no `src:` tag, which the seeded corpus and
        // the spec allow.
        assert!(entry.claims[3].sources.is_empty());
        assert_eq!(entry.claims[3].files.len(), 2);

        for claim in &entry.claims {
            assert!(claim.status.is_none());
            assert!(claim.superseded_by.is_none());
        }

        assert_eq!(render_entry(&entry), SEEDED_REPL);
    }

    #[test]
    fn index_is_regenerated_from_entries_with_counts() {
        let mut files = BTreeMap::new();
        files.insert("entries/repl.md".to_owned(), SEEDED_REPL.to_owned());
        let corpus = MemoryCorpus::parse(&files).unwrap();
        // The regenerated line uses the entry title and claim count; the
        // hand-written M0 description is not a format field (Q3).
        assert_eq!(
            corpus.render_index(),
            "# Memory Index\n\
             <!-- hard budget: ≤ 40 lines total. One line per entry. Regenerate, never hand-edit semantics. -->\n\
             - repl — REPL architecture (10 claims) — entries/repl.md\n"
        );
        assert_eq!(corpus.index_lines(), 3);
        assert_eq!(corpus.render_index().lines().count(), 3);
    }

    #[test]
    fn corpus_parse_rejects_a_file_slug_mismatch() {
        let mut files = BTreeMap::new();
        files.insert("entries/other.md".to_owned(), SEEDED_REPL.to_owned());
        assert!(matches!(
            MemoryCorpus::parse(&files),
            Err(CorpusParseError { .. })
        ));
    }

    #[test]
    fn append_updates_updated_run_and_preserves_old_lines() {
        let mut corpus = MemoryCorpus::default();
        corpus.insert(two_claim_entry());
        let before = render_entry(corpus.entry("repl").unwrap());

        let outcomes = corpus.apply_topic(
            &topic(
                "repl",
                vec![new_claim(
                    "New third fact.",
                    vec![msg(MSG_B)],
                    vec!["crates/agl-core/src/agent/memory_entry.rs".to_owned()],
                )],
            ),
            &run(RUN_C),
        );

        assert_eq!(outcomes, vec![ClaimApplyOutcome::Appended { ordinal: 3 }]);
        let entry = corpus.entry("repl").unwrap();
        assert_eq!(entry.created, run(RUN_A));
        assert_eq!(entry.updated, run(RUN_C));
        assert_eq!(entry.claims.len(), 3);
        // Newly applied claims carry paths without digests until the writer
        // stamps `@sha256-12` (task 4, M1 D3).
        assert!(entry.claims[2].files[0].digest.is_none());

        let after = render_entry(entry);
        // The `updated` run legitimately moves from RUN_B to RUN_C in the
        // header, so compare from the `## Claims` section down: the existing
        // claim lines stay byte-for-byte intact and the new claim is appended.
        let before_claims = before[before.find("## Claims").unwrap()..].to_owned();
        let after_claims = &after[after.find("## Claims").unwrap()..];
        assert!(
            after_claims.starts_with(&before_claims),
            "existing claim lines must stay untouched"
        );
        assert!(after.contains(&format!(
            "- New third fact. [src: {MSG_B}] [file: crates/agl-core/src/agent/memory_entry.rs]\n"
        )));
        assert_eq!(parse_entry(&after).unwrap(), entry.clone());
    }

    #[test]
    fn exact_text_duplicate_is_not_rewritten() {
        let mut corpus = MemoryCorpus::default();
        corpus.insert(two_claim_entry());
        let before = render_entry(corpus.entry("repl").unwrap());

        let outcomes = corpus.apply_topic(
            &topic(
                "repl",
                vec![new_claim("Old first fact.", vec![msg(MSG_B)], vec![])],
            ),
            &run(RUN_C),
        );

        assert_eq!(outcomes, vec![ClaimApplyOutcome::Duplicate { ordinal: 1 }]);
        let entry = corpus.entry("repl").unwrap();
        assert_eq!(render_entry(entry), before);
        assert_eq!(
            entry.updated,
            run(RUN_B),
            "a duplicate-only apply must not bump updated"
        );
    }

    #[test]
    fn supersession_tags_old_claim_without_rewriting_it() {
        let mut corpus = MemoryCorpus::default();
        corpus.insert(two_claim_entry());

        let outcomes = corpus.apply_topic(
            &topic(
                "repl",
                vec![superseding_claim(
                    "New first fact.",
                    vec![msg(MSG_B)],
                    vec!["repl#claim-1"],
                )],
            ),
            &run(RUN_C),
        );

        assert_eq!(outcomes, vec![ClaimApplyOutcome::Appended { ordinal: 3 }]);
        let entry = corpus.entry("repl").unwrap();
        assert_eq!(entry.claims[0].text, "Old first fact.");
        assert_eq!(
            entry.claims[0].superseded_by.as_deref(),
            Some("repl#claim-3")
        );
        assert_eq!(entry.claims[1].superseded_by, None);
        assert_eq!(entry.claims[2].text, "New first fact.");
        assert_eq!(entry.claims[2].superseded_by, None);

        let rendered = render_entry(entry);
        assert!(rendered.contains(&format!(
            "- Old first fact. [src: {MSG_A}] [superseded-by: repl#claim-3]\n"
        )));
        assert!(
            rendered
                .contains("- Old second fact. [file: crates/agl-core/src/lib.rs@3a4aa91013fd]\n")
        );
        assert!(rendered.contains(&format!("- New first fact. [src: {MSG_B}]\n")));
        assert_eq!(parse_entry(&rendered).unwrap(), entry.clone());
    }

    #[test]
    fn invalid_supersession_reference_rejects_the_new_claim() {
        for reference in [
            "repl#claim-3",
            "other#claim-1",
            "repl#claim-0",
            "repl#claim-",
        ] {
            let mut corpus = MemoryCorpus::default();
            corpus.insert(two_claim_entry());
            let before = render_entry(corpus.entry("repl").unwrap());

            let outcomes = corpus.apply_topic(
                &topic(
                    "repl",
                    vec![superseding_claim(
                        "New first fact.",
                        vec![msg(MSG_B)],
                        vec![reference],
                    )],
                ),
                &run(RUN_C),
            );

            assert_eq!(
                outcomes,
                vec![ClaimApplyOutcome::Rejected {
                    reason: MemoryClaimRejection::InvalidSupersession
                }],
                "reference {reference:?}"
            );
            assert_eq!(render_entry(corpus.entry("repl").unwrap()), before);
        }

        // A brand-new topic cannot supersede a claim that does not exist yet.
        let mut corpus = MemoryCorpus::default();
        let outcomes = corpus.apply_topic(
            &topic(
                "fresh",
                vec![superseding_claim(
                    "Fact.",
                    vec![msg(MSG_A)],
                    vec!["fresh#claim-1"],
                )],
            ),
            &run(RUN_C),
        );
        assert_eq!(
            outcomes,
            vec![ClaimApplyOutcome::Rejected {
                reason: MemoryClaimRejection::InvalidSupersession
            }]
        );
        assert!(corpus.entry("fresh").is_none());
    }

    #[test]
    fn new_topic_title_is_derived_from_slug() {
        assert_eq!(title_from_slug("build-release"), "Build release");
        assert_eq!(title_from_slug("repl"), "Repl");
        assert_eq!(title_from_slug("m1a"), "M1a");

        let mut corpus = MemoryCorpus::default();
        let outcomes = corpus.apply_topic(
            &topic(
                "build-release",
                vec![new_claim(
                    "The release train ships on Fridays.",
                    vec![msg(MSG_A)],
                    vec![],
                )],
            ),
            &run(RUN_C),
        );

        assert_eq!(outcomes, vec![ClaimApplyOutcome::Appended { ordinal: 1 }]);
        let entry = corpus.entry("build-release").unwrap();
        assert_eq!(entry.title, "Build release");
        assert_eq!(entry.created, run(RUN_C));
        assert_eq!(entry.updated, run(RUN_C));
        assert_eq!(entry.artifact, None);
        assert_eq!(
            corpus.render_index(),
            "# Memory Index\n\
             <!-- hard budget: ≤ 40 lines total. One line per entry. Regenerate, never hand-edit semantics. -->\n\
             - build-release — Build release (1 claims) — entries/build-release.md\n"
        );
    }

    #[test]
    fn new_topic_at_the_index_cap_is_rejected() {
        let mut files = BTreeMap::new();
        for index in 0..INDEX_MAX_LINES - 2 {
            let slug = format!("topic-{index:02}");
            files.insert(
                format!("entries/{slug}.md"),
                format!(
                    "# {}\n\n- slug: {slug}\n- created: {RUN_A}\n- updated: {RUN_A}\n\n## Claims\n\n- Fact. [src: {MSG_A}]\n",
                    title_from_slug(&slug)
                ),
            );
        }
        let mut corpus = MemoryCorpus::parse(&files).unwrap();
        // The cap counts the two header lines: 38 entries fill exactly 40.
        assert_eq!(corpus.len(), INDEX_MAX_LINES - 2);
        assert_eq!(corpus.index_lines(), INDEX_MAX_LINES);
        assert!(!corpus.fits_new_topic());

        let outcomes = corpus.apply_topic(
            &topic(
                "brand-new",
                vec![new_claim("Fact.", vec![msg(MSG_A)], vec![])],
            ),
            &run(RUN_C),
        );
        assert_eq!(
            outcomes,
            vec![ClaimApplyOutcome::Rejected {
                reason: MemoryClaimRejection::IndexCapReached
            }]
        );
        assert!(corpus.entry("brand-new").is_none());
        assert_eq!(corpus.render_index().lines().count(), INDEX_MAX_LINES);

        // Appending to an existing topic adds no index line and stays allowed.
        let outcomes = corpus.apply_topic(
            &topic(
                "topic-00",
                vec![new_claim("Another fact.", vec![msg(MSG_A)], vec![])],
            ),
            &run(RUN_C),
        );
        assert_eq!(outcomes, vec![ClaimApplyOutcome::Appended { ordinal: 2 }]);
        assert_eq!(corpus.index_lines(), INDEX_MAX_LINES);

        // One entry fewer: a new topic fits exactly at the cap.
        let mut files = BTreeMap::new();
        for index in 0..INDEX_MAX_LINES - 3 {
            let slug = format!("topic-{index:02}");
            files.insert(
                format!("entries/{slug}.md"),
                format!(
                    "# {}\n\n- slug: {slug}\n- created: {RUN_A}\n- updated: {RUN_A}\n\n## Claims\n\n- Fact. [src: {MSG_A}]\n",
                    title_from_slug(&slug)
                ),
            );
        }
        let mut corpus = MemoryCorpus::parse(&files).unwrap();
        assert!(corpus.fits_new_topic());
        let outcomes = corpus.apply_topic(
            &topic(
                "brand-new",
                vec![new_claim("Fact.", vec![msg(MSG_A)], vec![])],
            ),
            &run(RUN_C),
        );
        assert_eq!(outcomes, vec![ClaimApplyOutcome::Appended { ordinal: 1 }]);
        assert_eq!(corpus.index_lines(), INDEX_MAX_LINES);
    }

    #[test]
    fn optional_tags_round_trip() {
        let text = format!(
            "# Demo\n\n- slug: demo\n- created: {RUN_A}\n- updated: {RUN_B}\n- artifact: reports/demo.md\n\n## Claims\n\n\
             - Old fact. [src: {MSG_A}] [superseded-by: demo#claim-2]\n\
             - New fact. [file: notes/demo.md@3a4aa91013fd] [status: stale]\n"
        );
        let entry = parse_entry(&text).unwrap();
        assert_eq!(entry.claims.len(), 2);
        assert_eq!(
            entry.claims[0].superseded_by.as_deref(),
            Some("demo#claim-2")
        );
        assert_eq!(entry.claims[0].status, None);
        assert_eq!(
            entry.claims[1].files,
            vec![FileSource {
                path: "notes/demo.md".to_owned(),
                digest: Some("3a4aa91013fd".to_owned()),
            }]
        );
        assert_eq!(entry.claims[1].status, Some(ClaimStatus::Stale));
        assert_eq!(render_entry(&entry), text);
    }

    #[test]
    fn malformed_entries_are_rejected() {
        let base = format!(
            "# Demo\n\n- slug: demo\n- created: {RUN_A}\n- updated: {RUN_B}\n\n## Claims\n\n- Fact. [src: {MSG_A}]\n"
        );
        assert!(parse_entry(&base).is_ok());

        let missing_slug = base.replace("- slug: demo\n", "");
        assert!(matches!(
            parse_entry(&missing_slug),
            Err(EntryParseError::MissingField { ref field }) if field == "slug"
        ));

        let bad_run = base.replace(&format!("- created: {RUN_A}\n"), "- created: not-a-run\n");
        assert!(matches!(
            parse_entry(&bad_run),
            Err(EntryParseError::InvalidRun { .. })
        ));

        let unknown_status = base.replace(
            &format!("- Fact. [src: {MSG_A}]\n"),
            "- Fact. [status: fresh] [file: notes/demo.md]\n",
        );
        assert!(matches!(
            parse_entry(&unknown_status),
            Err(EntryParseError::UnknownStatus { .. })
        ));

        let unsourceful = base.replace(&format!("- Fact. [src: {MSG_A}]\n"), "- Fact.\n");
        assert!(matches!(
            parse_entry(&unsourceful),
            Err(EntryParseError::UnsourcefulClaim { .. })
        ));

        let no_claims = format!("# Demo\n\n- slug: demo\n- created: {RUN_A}\n- updated: {RUN_B}\n");
        assert!(matches!(
            parse_entry(&no_claims),
            Err(EntryParseError::MissingClaimsSection)
        ));

        let unknown_field = base.replace("- slug: demo\n", "- slug: demo\n- note: hi\n");
        assert!(matches!(
            parse_entry(&unknown_field),
            Err(EntryParseError::UnknownField { ref field, .. }) if field == "note"
        ));
    }
}
