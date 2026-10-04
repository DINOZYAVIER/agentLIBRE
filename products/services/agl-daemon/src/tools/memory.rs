use std::sync::{Arc, OnceLock, Weak};

use agl_core::Content;
use agl_core::agent::{ToolFailure, ToolFailureKind, ToolResult};
use agl_runtime::inference::{
    InferenceCancellation, InferenceHealthSink, InferenceRoute, InferenceServiceError,
    InferenceTokenizeRequest,
};
use agl_tool_api::{ExtensionBindings, ToolContext, ToolFuture, ToolHandler};
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::agent::memory_index::{MemoryIndex, SearchPage};
use crate::store::StoreHandle;

pub(crate) struct MemoryTools {
    pub(crate) index: Arc<MemoryIndex>,
    store: StoreHandle,
    inference: OnceLock<Weak<dyn InferenceRoute>>,
}
impl MemoryTools {
    pub(crate) fn new(index: Arc<MemoryIndex>, store: StoreHandle) -> Self {
        Self {
            index,
            store,
            inference: OnceLock::new(),
        }
    }
    pub(crate) fn initialize(&self, route: Arc<dyn InferenceRoute>) -> Result<()> {
        self.inference
            .set(Arc::downgrade(&route))
            .map_err(|_| anyhow::anyhow!("memory inference route already initialized"))
    }
    pub(crate) fn bindings(self: &Arc<Self>) -> Result<ExtensionBindings> {
        let view = agl_tools::memory::declaration();
        let package = agl_runtime::extension::parse_package_view(&view)?;
        agl_tools::memory::bindings(
            package.manifest.version,
            agl_core::package::compute_package_digest(&view)?,
            package.definition,
            self.clone(),
        )
    }
}
impl ToolHandler for MemoryTools {
    fn call(&self, context: ToolContext, input: Value) -> ToolFuture {
        let index = self.index.clone();
        let store = self.store.clone();
        let inference = self.inference.get().and_then(Weak::upgrade);
        let cancellation = InferenceCancellation::new();
        let cancel_on_drop = CancelOnDrop(cancellation.clone());
        Box::pin(async move {
            let _cancel_on_drop = cancel_on_drop;
            tokio::task::spawn_blocking(move || {
                check_context(&context)?;
                let input: SearchInput = serde_json::from_value(input).map_err(|_| invalid_input())?;
                if input.query.trim().is_empty() { return Err(invalid_input()); }
                let run = store.agent_run(context.operation.run_id).map_err(|_| unavailable("memory", None))?;
                if run.snapshot.workspace != context.workspace { return Err(unavailable("workspace", None)); }
                let fallback = agl_runtime::read_workspace_forge_declarations_at(context.workspace.root.as_path()).ok()
                    .and_then(|sources| sources.into_iter().find(|source| source.kind == "memory"))
                    .map(|source| format!("forge:{}/INDEX.md", source.id));
                let page = index.search(context.workspace.root.as_path(), context.operation.run_id, &input.query, input.limit.clamp(1,10))
                    .map_err(|error| { tracing::warn!(%error, "memory search unavailable"); unavailable("memory", fallback.as_deref()) })?;
                check_context(&context)?;
                let mut tokenizer_available = inference.is_some();
                let measure = |text: &str| -> Result<bool, ToolFailure> {
                    check_context(&context)?;
                    if tokenizer_available {
                        let route = inference.as_ref().expect("available inference route");
                        let health = { let store = store.clone(); InferenceHealthSink::new(move |updates| store.put_inference_health_updates(&updates).map_err(|_| ())) };
                        match route.count_text_tokens(InferenceTokenizeRequest {
                            operation: context.operation.clone(), model: run.snapshot.model.model.clone(), runtime: run.snapshot.model.runtime.clone(), text: text.to_owned(),
                            deadline_at_ms: context.deadline_at_ms, cancellation: cancellation.clone(), health: Some(health),
                        }) {
                            Ok(count) => { check_context(&context)?; return Ok(count <= 600); }
                            Err(InferenceServiceError::Cancelled) => return Err(ToolFailure::no_effect(ToolFailureKind::Cancelled, None, &[])),
                            Err(InferenceServiceError::Deadline) => return Err(ToolFailure::no_effect(ToolFailureKind::Deadline, None, &[])),
                            Err(error) => { tracing::debug!(%error, "memory tokenizer unavailable; using 600-byte budget"); tokenizer_available = false; }
                        }
                    }
                    Ok(text.len() <= 600)
                };
                let text = fit_page(page, measure)?;
                let result = ToolResult { content: Content::text(text).map_err(|_| unavailable("memory", fallback.as_deref()))?, effect_receipts: vec![] };
                if serde_json::to_vec(&result).map_err(|_| unavailable("memory", fallback.as_deref()))?.len() as u64 > context.result_bytes {
                    return Err(too_large(fallback.as_deref().unwrap_or("Forge memory INDEX")));
                }
                check_context(&context)?;
                Ok(result)
            }).await.map_err(|_| unavailable("memory", None))?
        })
    }
}

struct CancelOnDrop(InferenceCancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    query: String,
    limit: i64,
}

fn check_context(context: &ToolContext) -> Result<(), ToolFailure> {
    if context.is_cancelled() {
        return Err(ToolFailure::no_effect(
            ToolFailureKind::Cancelled,
            None,
            &[],
        ));
    }
    if context.deadline_at_ms <= crate::agent::now_ms() {
        return Err(ToolFailure::no_effect(ToolFailureKind::Deadline, None, &[]));
    }
    Ok(())
}
fn invalid_input() -> ToolFailure {
    ToolFailure::no_effect(
        ToolFailureKind::InvalidInput,
        Some("query"),
        &["Provide a nonempty query."],
    )
}
fn unavailable(field: &str, index: Option<&str>) -> ToolFailure {
    let action = format!(
        "Read {} with artifact.read and use rg to investigate the relevant topic.",
        index.unwrap_or("the Forge memory INDEX")
    );
    ToolFailure::no_effect(ToolFailureKind::Unavailable, Some(field), &[&action])
}
fn too_large(path: &str) -> ToolFailure {
    ToolFailure::no_effect(
        ToolFailureKind::ResultTooLarge,
        None,
        &[&format!("Read {path} directly with artifact.read.")],
    )
}

fn format_page(page: &SearchPage) -> String {
    let mut text = if page.hits.is_empty() {
        format!(
            "No matching claims. {}:\n{}",
            page.index_path, page.fallback
        )
    } else {
        String::new()
    };
    for (index, hit) in page.hits.iter().enumerate() {
        text.push_str(&format!(
            "{}. [{:.6}] {} — {} [src: {}]",
            index + 1,
            hit.score,
            hit.slug,
            hit.text,
            hit.sources
        ));
        if let Some(target) = &hit.supersession {
            text.push_str(&format!(" [superseded-by: {target}]"));
        }
        match (hit.stale, hit.superseded) {
            (true, true) => text.push_str(" [status: stale, superseded]"),
            (true, false) => text.push_str(" [status: stale]"),
            (false, true) => text.push_str(" [status: superseded]"),
            _ => {}
        }
        text.push('\n');
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("Total claims: {}\n", page.total));
    if !page.skipped.is_empty() {
        text.push_str(&format!("Skipped files: {}\n", page.skipped.join(", ")));
    }
    if !page.unchecked.is_empty() {
        text.push_str(&format!(
            "Unchecked Forge files: {}\n",
            page.unchecked.join(", ")
        ));
    }
    text
}
fn fit_page(
    mut page: SearchPage,
    mut fits: impl FnMut(&str) -> Result<bool, ToolFailure>,
) -> Result<String, ToolFailure> {
    if page.hits.is_empty() {
        return Ok(format_page(&page));
    }
    loop {
        let text = format_page(&page);
        if fits(&text)? {
            return Ok(text);
        }
        if page.hits.len() > 1 {
            page.hits.pop();
            continue;
        }
        let original = page.hits[0].text.clone();
        let path = format!(
            "{}/entries/{}.md",
            page.index_path.trim_end_matches("/INDEX.md"),
            page.hits[0].slug
        );
        page.hits[0].text = "[…truncated]".into();
        if !fits(&format_page(&page))? {
            return Err(too_large(&path));
        }
        let boundaries = std::iter::once(0)
            .chain(
                original
                    .char_indices()
                    .filter_map(|(index, ch)| ch.is_whitespace().then_some(index)),
            )
            .collect::<Vec<_>>();
        let mut lower = 0;
        let mut upper = boundaries.len();
        while lower + 1 < upper {
            let middle = (lower + upper) / 2;
            page.hits[0].text =
                format!("{} […truncated]", original[..boundaries[middle]].trim_end());
            if fits(&format_page(&page))? {
                lower = middle;
            } else {
                upper = middle;
            }
        }
        page.hits[0].text = format!(
            "{}{}[…truncated]",
            original[..boundaries[lower]].trim_end(),
            if boundaries[lower] > 0 { " " } else { "" }
        );
        let text = format_page(&page);
        if fits(&text)? {
            return Ok(text);
        }
        return Err(too_large(&path));
    }
}

#[cfg(test)]
mod tests;
