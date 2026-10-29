//! The LocalPDF-TUI engine.
//!
//! This module is the only place that touches the filesystem or PDF objects.
//! It has two job categories:
//!
//! 1. **Sandboxed I/O.** Every path — human-typed in the TUI or agent-passed
//!    via the CLI — is treated as untrusted input. `resolve()` canonicalizes
//!    it (resolving symlinks and `..` segments) and then pins it under the
//!    workspace root; anything that would escape is rejected with
//!    `AppError::PathEscape` before a byte is read.
//!
//! 2. **PDF primitives.** Merge and split are implemented as object-closure
//!    copies between `lopdf::Document`s. `lopdf` parses each source via its
//!    xref table into a flat object table, so no whole-file byte image is
//!    re-buffered per operation; only the objects reachable from the pages
//!    we keep are copied into the output document. Dangling references found
//!    in damaged inputs are materialized as `Null` placeholders so the
//!    output document is always self-consistent.

use lopdf::dictionary;
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};

/// Deterministic success payload for agent mode. Struct fields serialize in
/// declaration order, so the byte layout of the document is stable.
#[derive(Debug, Serialize)]
pub struct OperationReport {
    pub status: &'static str,
    pub operation: &'static str,
    pub output_file: String,
    pub pages: u32,
}

/// Inclusive 1-based page range: `start..=end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: u32,
    pub end: u32,
}

/// Parse a spec like `1-3,5,7-9` into inclusive ranges.
///
/// A lone number is a single-page range. Numbers are validated here; the
/// caller checks bounds against the document's page count.
pub fn parse_ranges(spec: &str) -> AppResult<Vec<Range>> {
    let mut out = Vec::new();
    for token in spec.split(',') {
        let token = token.trim();
        if token.is_empty() {
            return Err(AppError::PageRange(format!(
                "empty token in range spec '{spec}'"
            )));
        }
        let (a, b) = match token.split_once('-') {
            Some((a, b)) => (a, b),
            None => (token, token),
        };
        let start: u32 = a
            .trim()
            .parse()
            .map_err(|_| AppError::PageRange(format!("'{}' is not a page number", a.trim())))?;
        let end: u32 = b
            .trim()
            .parse()
            .map_err(|_| AppError::PageRange(format!("'{}' is not a page number", b.trim())))?;
        if start == 0 || start > end {
            return Err(AppError::PageRange(format!(
                "'{token}' is not a valid page range (want 1-based, start <= end)"
            )));
        }
        out.push(Range { start, end });
    }
    Ok(out)
}

/// Sandboxed engine: every file access is pinned under `root`.
#[derive(Debug, Clone)]
pub struct Engine {
    root: PathBuf,
}

impl Engine {
    /// Create an engine whose sandbox is `workspace` (default: CWD).
    pub fn new(workspace: &Path) -> AppResult<Self> {
        let root = fs::canonicalize(workspace)
            .map_err(|e| AppError::Workspace(workspace.display().to_string(), e))?;
        Ok(Self { root })
    }

    #[allow(dead_code)] // part of the engine API; exercised by tests
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve an untrusted path and pin it inside the sandbox.
    ///
    /// Canonicalization resolves `..` segments and symlinks; the
    /// `starts_with(root)` check then guarantees the result cannot point
    /// anywhere the user did not designate as the workspace. Use for files
    /// that already exist (inputs). For destinations we create ourselves,
    /// use `resolve_destination`.
    pub fn resolve(&self, untrusted: &Path) -> AppResult<PathBuf> {
        let candidate = if untrusted.is_absolute() {
            untrusted.to_path_buf()
        } else {
            self.root.join(untrusted)
        };
        let canonical = fs::canonicalize(&candidate).map_err(|_| AppError::PathEscape {
            given: candidate.display().to_string(),
        })?;
        if !canonical.starts_with(&self.root) {
            return Err(AppError::PathEscape {
                given: candidate.display().to_string(),
            });
        }
        Ok(canonical)
    }

    /// Pin a destination we are about to create (outputs, generated samples)
    /// inside the sandbox.
    ///
    /// The file itself does not exist yet, so canonicalizing it would fail;
    /// we canonicalize its parent directory instead — which does exist and
    /// gets the same containment check — then re-attach the leaf name.
    pub fn resolve_destination(&self, untrusted: &Path) -> AppResult<PathBuf> {
        let candidate = if untrusted.is_absolute() {
            untrusted.to_path_buf()
        } else {
            self.root.join(untrusted)
        };
        let name = candidate
            .file_name()
            .ok_or_else(|| AppError::PathEscape {
                given: candidate.display().to_string(),
            })?;
        let parent = candidate
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| AppError::PathEscape {
                given: candidate.display().to_string(),
            })?
            .to_path_buf();
        let canonical_parent = fs::canonicalize(&parent).map_err(|_| AppError::PathEscape {
            given: candidate.display().to_string(),
        })?;
        if !canonical_parent.starts_with(&self.root) {
            return Err(AppError::PathEscape {
                given: candidate.display().to_string(),
            });
        }
        Ok(canonical_parent.join(name))
    }

    fn load_pdf(&self, path: &Path) -> AppResult<Document> {
        let resolved = self.resolve(path)?;
        Document::load(&resolved).map_err(|e| AppError::Pdf(e.to_string()))
    }

    /// Merge `inputs` (in order) into a single document written to `output`.
    ///
    /// Each source document's object closure (everything reachable from its
    /// page tree root) is copied into the output with fresh object ids and
    /// rewritten references; the page trees are then hung off one shared
    /// root node, so the merged document's page order is input order.
    pub fn merge(&self, inputs: &[PathBuf], output: &Path) -> AppResult<OperationReport> {
        if inputs.is_empty() {
            return Err(AppError::Operation("merge requires at least one input file".into()));
        }

        let mut merged = Document::with_version("1.7");
        let root_pages = merged.new_object_id();
        let mut kids: Vec<ObjectId> = Vec::new();
        let mut total_pages = 0u32;

        for input in inputs {
            let src = self.load_pdf(input)?;
            total_pages = total_pages.saturating_add(src.get_pages().len() as u32);
            let src_root = Self::root_pages_id(&src)?;

            let closure = Self::collect_closure(&src, src_root, true);
            let mut map: HashMap<ObjectId, ObjectId> = HashMap::new();
            for &id in &closure {
                map.insert(id, merged.new_object_id());
            }
            for &id in &closure {
                let obj = src.objects.get(&id).cloned().unwrap_or(Object::Null);
                let copied = Self::remap_object(&obj, &src, &mut map, &mut merged);
                merged.objects.insert(map[&id], copied);
            }

            // The copied source root now hangs off our new root; fix its
            // /Parent link so the tree stays well-formed.
            if let Some(Object::Dictionary(d)) = merged.objects.get_mut(&map[&src_root]) {
                d.set("Parent", root_pages);
            }
            kids.push(map[&src_root]);
        }

        merged.objects.insert(
            root_pages,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => Object::Array(kids.iter().map(|k| Object::Reference(*k)).collect()),
                "Count" => Object::Integer(kids.len() as i64),
            }),
        );

        let catalog_id = merged.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => root_pages,
        });
        merged.trailer.set("Root", catalog_id);

        let written = self.save_pdf(&mut merged, output)?;
        Ok(OperationReport {
            status: "success",
            operation: "merge",
            output_file: written.display().to_string(),
            pages: total_pages,
        })
    }

    /// Extract `ranges` (1-based, inclusive) from `input` into `output`.
    ///
    /// Only the objects reachable from the selected pages are copied.
    /// `/Parent` links are deliberately not followed during the traversal:
    /// following them would pull the whole page tree — and with it every
    /// unselected sibling page — into the output. Parent links are rewritten
    /// to the new root after the copy.
    pub fn split(
        &self,
        input: &Path,
        ranges: &[Range],
        output: &Path,
    ) -> AppResult<OperationReport> {
        let src = self.load_pdf(input)?;
        let pages: BTreeMap<u32, ObjectId> = src.get_pages();
        let total = pages.len() as u32;
        if total == 0 {
            return Err(AppError::Operation("document contains no pages".into()));
        }

        let mut used: HashSet<u32> = HashSet::new();
        let mut selected: Vec<ObjectId> = Vec::new();
        for r in ranges {
            if r.start > total {
                return Err(AppError::PageRange(format!(
                    "page {} is beyond the end of a {}-page document",
                    r.start, total
                )));
            }
            for n in r.start..=r.end.min(total) {
                let id = *pages
                    .get(&n)
                    .ok_or_else(|| AppError::PageRange(format!("page {n} missing from page tree")))?;
                if used.insert(n) {
                    selected.push(id);
                }
            }
        }

        let mut merged = Document::with_version("1.7");
        let root_pages = merged.new_object_id();

        let mut seen: HashSet<ObjectId> = HashSet::new();
        let mut closure: Vec<ObjectId> = Vec::new();
        for &page in &selected {
            Self::expand_closure(&src, page, false, &mut seen, &mut closure);
        }
        let mut map: HashMap<ObjectId, ObjectId> = HashMap::new();
        for &id in &closure {
            map.insert(id, merged.new_object_id());
        }
        for &id in &closure {
            let obj = src.objects.get(&id).cloned().unwrap_or(Object::Null);
            let copied = Self::remap_object(&obj, &src, &mut map, &mut merged);
            merged.objects.insert(map[&id], copied);
        }
        for &page in &selected {
            if let Some(Object::Dictionary(d)) = merged.objects.get_mut(&map[&page]) {
                d.set("Parent", root_pages);
            }
        }

        // Carry the media box from the source root when present so viewers
        // fall back to the document's own page size instead of a default.
        let media_box = Self::root_pages_id(&src)
            .ok()
            .and_then(|id| src.get_dictionary(id).ok())
            .and_then(|d| d.get(b"MediaBox").ok())
            .cloned();
        let mut root_dict = dictionary! {
            "Type" => "Pages",
            "Kids" => Object::Array(selected.iter().map(|id| Object::Reference(map[id])).collect()),
            "Count" => Object::Integer(selected.len() as i64),
        };
        if let Some(box_obj) = media_box {
            root_dict.set("MediaBox", box_obj);
        }
        merged.objects.insert(root_pages, Object::Dictionary(root_dict));

        let catalog_id = merged.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => root_pages,
        });
        merged.trailer.set("Root", catalog_id);

        let written = self.save_pdf(&mut merged, output)?;
        Ok(OperationReport {
            status: "success",
            operation: "split",
            output_file: written.display().to_string(),
            pages: selected.len() as u32,
        })
    }

    /// Create a single-page sample document at `name` (workspace-relative).
    /// Backs the TUI `[a]` key so merge/split can be demoed with zero
    /// external input files.
    pub fn generate_sample(&self, name: &str, label: &str) -> AppResult<PathBuf> {
        let target = self.resolve_destination(Path::new(name))?;

        let mut doc = Document::with_version("1.7");
        let info_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal(label),
            "Creator" => Object::string_literal("LocalPDF-TUI"),
        });
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content = format!(
            "BT /F1 24 Tf 72 700 Td ({}) Tj ET",
            escape_pdf_string(label)
        )
        .into_bytes();
        let content_id = doc.add_object(Stream::new(dictionary! {}, content));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Contents" => content_id,
            "Resources" => resources_id,
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => Object::Array(vec![Object::Reference(page_id)]),
                "Count" => Object::Integer(1),
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        doc.trailer.set("Info", info_id);
        doc.save(&target).map_err(|e| AppError::Pdf(e.to_string()))?;
        Ok(target)
    }

    fn save_pdf(&self, doc: &mut Document, path: &Path) -> AppResult<PathBuf> {
        let resolved = self.resolve_destination(path)?;
        doc.save(&resolved).map_err(|e| AppError::Pdf(e.to_string()))?;
        Ok(resolved)
    }

    /// The object id of the root of a document's page tree.
    fn root_pages_id(doc: &Document) -> AppResult<ObjectId> {
        let catalog = doc.catalog().map_err(|e| AppError::Pdf(e.to_string()))?;
        let root = catalog
            .get(b"Pages")
            .map_err(|_| AppError::Pdf("document has no /Pages entry".into()))?;
        match root {
            Object::Reference(id) => Ok(*id),
            _ => Err(AppError::Pdf("/Pages is not an indirect reference".into())),
        }
    }

    /// All object ids reachable from `start`, in traversal order.
    /// With `follow_parent` set, every reference is walked; otherwise
    /// `/Parent` references are skipped (split's minimal-copy mode).
    fn collect_closure(doc: &Document, start: ObjectId, follow_parent: bool) -> Vec<ObjectId> {
        let mut seen: HashSet<ObjectId> = HashSet::new();
        let mut out: Vec<ObjectId> = Vec::new();
        Self::expand_closure(doc, start, follow_parent, &mut seen, &mut out);
        out
    }

    fn expand_closure(
        doc: &Document,
        start: ObjectId,
        follow_parent: bool,
        seen: &mut HashSet<ObjectId>,
        out: &mut Vec<ObjectId>,
    ) {
        let mut stack = vec![start];
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            out.push(id);
            if let Some(obj) = doc.objects.get(&id) {
                for r in Self::refs_in(obj, follow_parent) {
                    if !seen.contains(&r) {
                        stack.push(r);
                    }
                }
            }
        }
    }

    /// Every indirect reference contained in `obj`.
    fn refs_in(obj: &Object, follow_parent: bool) -> Vec<ObjectId> {
        fn collect(v: &Object, follow_parent: bool, out: &mut Vec<ObjectId>) {
            match v {
                Object::Reference(id) => out.push(*id),
                Object::Array(items) => {
                    for item in items {
                        collect(item, follow_parent, out);
                    }
                }
                Object::Dictionary(d) => {
                    for (key, value) in d.iter() {
                        if !follow_parent && key.as_slice() == b"Parent" {
                            continue;
                        }
                        collect(value, follow_parent, out);
                    }
                }
                Object::Stream(s) => {
                    for (_, value) in s.dict.iter() {
                        collect(value, follow_parent, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        collect(obj, follow_parent, &mut out);
        out
    }

    /// Clone `obj` into `dst`, rewriting every reference through `map`.
    ///
    /// References outside the copied closure are materialized lazily: a
    /// defined object gets its own copy, a dangling one a `Null`
    /// placeholder, so the output document never contains unresolvable
    /// references.
    fn remap_object(
        obj: &Object,
        src: &Document,
        map: &mut HashMap<ObjectId, ObjectId>,
        dst: &mut Document,
    ) -> Object {
        fn remap_value(
            v: &Object,
            src: &Document,
            map: &mut HashMap<ObjectId, ObjectId>,
            dst: &mut Document,
        ) -> Object {
            match v {
                Object::Reference(id) => {
                    if let Some(&new) = map.get(id) {
                        return Object::Reference(new);
                    }
                    let new = dst.new_object_id();
                    map.insert(*id, new);
                    let target = match src.objects.get(id) {
                        Some(o) => o.clone(),
                        None => Object::Null,
                    };
                    let copied = remap_value(&target, src, map, dst);
                    dst.objects.insert(new, copied);
                    Object::Reference(new)
                }
                Object::Array(items) => Object::Array(
                    items
                        .iter()
                        .map(|item| remap_value(item, src, map, dst))
                        .collect(),
                ),
                Object::Dictionary(d) => {
                    let mut nd = Dictionary::new();
                    for (key, value) in d.iter() {
                        nd.set(key.clone(), remap_value(value, src, map, dst));
                    }
                    Object::Dictionary(nd)
                }
                Object::Stream(s) => {
                    let mut nd = s.dict.clone();
                    for (_, value) in nd.iter_mut() {
                        *value = remap_value(value, src, map, dst);
                    }
                    Object::Stream(Stream {
                        dict: nd,
                        content: s.content.clone(),
                        allows_compression: s.allows_compression,
                        start_position: s.start_position,
                    })
                }
                _ => v.clone(),
            }
        }
        remap_value(obj, src, map, dst)
    }
}

/// Escape a label for embedding in a PDF literal string.
fn escape_pdf_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('(', "\\(").replace(')', "\\)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("localpdf-tui-tests-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn three_samples() -> (Engine, Vec<PathBuf>) {
        let root = temp_workspace("merge");
        let engine = Engine::new(&root).unwrap();
        let mut files = Vec::new();
        for i in 1..=3 {
            files.push(
                engine
                    .generate_sample(&format!("sample_{i}.pdf"), &format!("Sample {i}"))
                    .unwrap(),
            );
        }
        (engine, files)
    }

    #[test]
    fn merge_combines_page_counts() {
        let (engine, files) = three_samples();
        let report = engine.merge(&files, std::path::Path::new("merged.pdf")).unwrap();
        assert_eq!(report.pages, 3);
        let loaded = Document::load(engine.root().join("merged.pdf")).unwrap();
        assert_eq!(loaded.get_pages().len(), 3);
    }

    #[test]
    fn split_reselects_single_page() {
        let (engine, files) = three_samples();
        let report = engine
            .split(&files[0], &parse_ranges("1").unwrap(), std::path::Path::new("split.pdf"))
            .unwrap();
        assert_eq!(report.pages, 1);
        let loaded = Document::load(engine.root().join("split.pdf")).unwrap();
        assert_eq!(loaded.get_pages().len(), 1);
    }

    #[test]
    fn split_rejects_out_of_bounds() {
        let (engine, files) = three_samples();
        let err = engine
            .split(&files[0], &parse_ranges("4-9").unwrap(), std::path::Path::new("x.pdf"))
            .unwrap_err();
        assert!(matches!(err, AppError::PageRange(_)));
    }

    #[test]
    fn parse_ranges_round_trips() {
        let ranges = parse_ranges("1-3,5,7-9").unwrap();
        assert_eq!(
            ranges,
            vec![
                Range { start: 1, end: 3 },
                Range { start: 5, end: 5 },
                Range { start: 7, end: 9 },
            ]
        );
        assert!(parse_ranges("0-2").is_err());
        assert!(parse_ranges("3-1").is_err());
        assert!(parse_ranges("a-b").is_err());
    }

    #[test]
    fn path_escape_is_rejected() {
        let sandbox = temp_workspace("esc-a");
        let engine = Engine::new(&sandbox).unwrap();
        let outside = temp_workspace("esc-b");
        let outside_file = outside.join("secret.pdf");
        fs::write(&outside_file, b"%PDF-1.4\n").unwrap();
        let err = engine.resolve(&outside_file).unwrap_err();
        assert!(matches!(err, AppError::PathEscape { .. }));
    }
}
