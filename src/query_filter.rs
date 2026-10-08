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
//! `Arc`). So the closure only captures the query string, and each thread
//! that calls it keeps its own engine and compiled program in a
//! thread-local, built on its first call. Building an engine costs far
//! more than evaluating a query, so this matters: the filter runs once per
//! heading on every (re)load. The query is validated once eagerly here so
//! a typo is reported at startup rather than silently matching nothing
//! later.
use std::cell::RefCell;
use std::sync::Arc;

use mq_markdown::Node;

use crate::document::HeadingFilter;

struct Compiled {
    query: String,
    engine: mq_lang::DefaultEngine,
    program: mq_lang::CompiledProgram,
}

thread_local! {
    static COMPILED: RefCell<Option<Compiled>> = const { RefCell::new(None) };
}

fn run(query: &str, node: &Node) -> Option<bool> {
    COMPILED.with_borrow_mut(|slot| {
        if slot.as_ref().is_none_or(|c| c.query != query) {
            let mut engine = mq_lang::DefaultEngine::default();
            engine.load_builtin_module();
            let program = engine.compile(query).ok()?;
            *slot = Some(Compiled {
                query: query.to_string(),
                engine,
                program,
            });
        }
        let Compiled { engine, program, .. } = slot.as_mut()?;
        let value = mq_lang::RuntimeValue::from(node.clone());
        let result = engine.eval_compiled(program, std::iter::once(value)).ok()?;
        Some(!result.compact().is_empty())
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;

    fn kept(query: &str, src: &str) -> Vec<String> {
        let doc = Document::parse(src).unwrap();
        let filter = compile(query).unwrap();
        let tree = doc.tree_filtered(&*filter);
        let mut out = Vec::new();
        fn walk(s: &crate::document::Section, out: &mut Vec<String>) {
            for c in &s.children {
                out.push(c.name.clone());
                walk(c, out);
            }
        }
        walk(&tree.root, &mut out);
        out
    }

    #[test]
    fn rejects_an_invalid_query_up_front() {
        assert!(compile("select(").is_err());
    }

    #[test]
    fn the_cached_engine_gives_the_same_answer_on_every_call() {
        let src = "# A\n\n## TODO one\n\n## Done\n\n## TODO two\n";
        let expected = vec!["A", "TODO one", "TODO two"];
        for _ in 0..3 {
            assert_eq!(kept(r#"select(contains("TODO"))"#, src), expected);
        }
        // Switching queries on the same thread must not reuse the old program.
        assert_eq!(kept(".h1", src), vec!["A"]);
        assert_eq!(kept(r#"select(contains("TODO"))"#, src), expected);
    }

    #[test]
    fn works_from_several_threads_at_once() {
        let filter = compile(".h2").unwrap();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let filter = Arc::clone(&filter);
                std::thread::spawn(move || {
                    let doc = Document::parse("# A\n\n## B\n\n### C\n").unwrap();
                    let tree = doc.tree_filtered(&*filter);
                    tree.find(&["A", "B"]).is_some() && tree.find(&["A", "B", "C"]).is_none()
                })
            })
            .collect();
        assert!(handles.into_iter().all(|h| h.join().unwrap()));
    }
}
