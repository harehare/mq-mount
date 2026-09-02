//! Compiles a `--filter` query string (mq's own jq-like Markdown query
//! language, crate `mq-lang`) into a [`crate::document::HeadingFilter`]
//! closure `fs.rs`/`document.rs` can call without knowing anything about the
//! query engine — see [`crate::document::HeadingFilter`]'s doc comment for
//! why that boundary matters (the core must stay buildable without the
//! `mount` feature).
//!
//! `mq_lang::Engine`/`CompiledProgram` are `Rc`-based and therefore not
//! `Send`/`Sync`, while [`crate::document::HeadingFilter`] must be (it's
//! stored in `fs::MountState`, shared across the mount's worker threads via
//! `Arc`). So a fresh engine is compiled and dropped inside every single
//! call instead of being kept alive across calls; the closure's only
//! captured state is the query string itself, which is trivially
//! `Send + Sync`. The query is validated once eagerly here so a typo is
//! reported at startup rather than silently matching nothing later.
use std::sync::Arc;

use mq_markdown::Node;

use crate::document::HeadingFilter;

fn run(query: &str, node: &Node) -> Option<bool> {
    let mut engine = mq_lang::DefaultEngine::default();
    engine.load_builtin_module();
    let compiled = engine.compile(query).ok()?;
    let value = mq_lang::RuntimeValue::from(node.clone());
    let result = engine.eval_compiled(&compiled, std::iter::once(value)).ok()?;
    Some(!result.compact().is_empty())
}

/// Validates `query` once and returns a [`HeadingFilter`] closure that
/// re-evaluates it against each heading node it's called with.
pub fn compile(query: &str) -> miette::Result<Arc<HeadingFilter>> {
    let mut engine = mq_lang::DefaultEngine::default();
    engine.load_builtin_module();
    engine
        .compile(query)
        .map_err(|e| miette::Report::new(*e).wrap_err("invalid --filter query"))?;

    let query = query.to_string();
    Ok(Arc::new(move |node: &Node| run(&query, node).unwrap_or(false)))
}
