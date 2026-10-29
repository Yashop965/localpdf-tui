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

/// Read-only document report for the `info` operation.
#[derive(Debug, Serialize)]
pub struct InfoReport {
    pub status: &'static str,
    pub operation: &'static str,
    pub source_file: String,
    pub pages: u32,
    pub file_size_bytes: u64,
    pub object_count: usize,
    pub encrypted: bool,
    pub title: Option<String>,
    pub creator: Option<String>,
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
            output_file: display_path(&written),
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
        let selected = Self::select_pages(&src, ranges)?;
        if selected.is_empty() {
            return Err(AppError::Operation("no pages matched the range spec".into()));
        }

        let mut doc = Self::build_extracted_document(&src, &selected)?;
        let written = self.save_pdf(&mut doc, output)?;
        Ok(OperationReport {
            status: "success",
            operation: "split",
            output_file: display_path(&written),
            pages: selected.len() as u32,
        })
    }

    /// Delete the pages selected by `ranges_spec` (e.g. `2,4-5`), keeping the
    /// rest in document order. The inverse of split — same minimal-copy
    /// machinery, applied to the complement.
    pub fn delete_pages(
        &self,
        input: &Path,
        ranges_spec: &str,
        output: &Path,
    ) -> AppResult<OperationReport> {
        let ranges = parse_ranges(ranges_spec)?;
        let src = self.load_pdf(input)?;
        let pages_map = src.get_pages();
        let total = pages_map.len() as u32;
        if total == 0 {
            return Err(AppError::Operation("document contains no pages".into()));
        }

        let mut drop: HashSet<u32> = HashSet::new();
        for r in &ranges {
            if r.start > total {
                return Err(AppError::PageRange(format!(
                    "page {} is beyond the end of a {}-page document",
                    r.start, total
                )));
            }
            drop.extend(r.start..=r.end.min(total));
        }

        let mut keep: Vec<ObjectId> = Vec::new();
        for n in 1..=total {
            if !drop.contains(&n) {
                keep.push(
                    *pages_map
                        .get(&n)
                        .ok_or_else(|| AppError::PageRange(format!("page {n} missing from page tree")))?,
                );
            }
        }
        if keep.is_empty() {
            return Err(AppError::Operation("cannot delete every page of a document".into()));
        }

        let mut doc = Self::build_extracted_document(&src, &keep)?;
        let written = self.save_pdf(&mut doc, output)?;
        Ok(OperationReport {
            status: "success",
            operation: "delete",
            output_file: display_path(&written),
            pages: keep.len() as u32,
        })
    }

    /// Rotate the pages selected by `ranges_spec` by `degrees` (90/180/270),
    /// writing the result to `output`. All other pages carry over unchanged.
    pub fn rotate(
        &self,
        input: &Path,
        ranges_spec: &str,
        degrees: u32,
        output: &Path,
    ) -> AppResult<OperationReport> {
        match degrees {
            90 | 180 | 270 => {}
            _ => {
                return Err(AppError::PageRange(format!(
                    "--degrees must be 90, 180, or 270 (got {degrees})"
                )))
            }
        }
        let ranges = parse_ranges(ranges_spec)?;
        let mut src = self.load_pdf(input)?;
        let pages_map = src.get_pages();
        let total = pages_map.len() as u32;
        if total == 0 {
            return Err(AppError::Operation("document contains no pages".into()));
        }

        let mut rotated = 0u32;
        for r in &ranges {
            if r.start > total {
                return Err(AppError::PageRange(format!(
                    "page {} is beyond the end of a {}-page document",
                    r.start, total
                )));
            }
            for n in r.start..=r.end.min(total) {
                let id = *pages_map
                    .get(&n)
                    .ok_or_else(|| AppError::PageRange(format!("page {n} missing from page tree")))?;
                src.get_dictionary_mut(id)
                    .map_err(|e| AppError::Pdf(e.to_string()))?
                    .set("Rotate", Object::Integer(degrees as i64));
                rotated += 1;
            }
        }
        if rotated == 0 {
            return Err(AppError::Operation("no pages matched the range spec".into()));
        }

        let written = self.save_pdf(&mut src, output)?;
        Ok(OperationReport {
            status: "success",
            operation: "rotate",
            output_file: display_path(&written),
            pages: rotated,
        })
    }

    /// Reorder every page: `order_spec` must list all page numbers exactly
    /// once (e.g. `3,1,2`). The output keeps the full document, with the
    /// root page tree rebuilt in the requested order.
    pub fn reorder(&self, input: &Path, order_spec: &str, output: &Path) -> AppResult<OperationReport> {
        let src = self.load_pdf(input)?;
        let pages_map = src.get_pages();
        let total = pages_map.len() as u32;
        if total == 0 {
            return Err(AppError::Operation("document contains no pages".into()));
        }

        let spec = order_spec.trim();
        if spec.is_empty() {
            return Err(AppError::Operation(
                "--order must list every page exactly once, e.g. 3,1,2".into(),
            ));
        }
        let mut nums: Vec<u32> = Vec::new();
        for token in spec.split(',') {
            let token = token.trim();
            let n: u32 = token
                .parse()
                .map_err(|_| AppError::PageRange(format!("'{}' is not a page number", token)))?;
            if n == 0 || n > total {
                return Err(AppError::PageRange(format!(
                    "page {n} is outside 1..{total}"
                )));
            }
            nums.push(n);
        }
        if nums.len() != total as usize {
            return Err(AppError::Operation(format!(
                "--order must contain exactly {total} entries (one per page, found {})",
                nums.len()
            )));
        }
        let mut seen: HashSet<u32> = HashSet::new();
        for &n in &nums {
            if !seen.insert(n) {
                return Err(AppError::Operation(format!("page {n} appears twice in --order")));
            }
        }

        let mut ordered = Vec::with_capacity(nums.len());
        for &n in &nums {
            ordered.push(
                *pages_map
                    .get(&n)
                    .ok_or_else(|| AppError::PageRange(format!("page {n} missing from page tree")))?,
            );
        }

        let mut doc = Self::build_extracted_document(&src, &ordered)?;
        let written = self.save_pdf(&mut doc, output)?;
        Ok(OperationReport {
            status: "success",
            operation: "reorder",
            output_file: display_path(&written),
            pages: total,
        })
    }

    /// Compress: keep only objects reachable from the catalog (orphaned
    /// objects from incremental updates are dropped) and Flate-compress
    /// every unfiltered stream that gets smaller.
    pub fn compress(&self, input: &Path, output: &Path) -> AppResult<OperationReport> {
        let src = self.load_pdf(input)?;
        let root_ref = src
            .trailer
            .get(b"Root")
            .map_err(|e| AppError::Pdf(e.to_string()))?;
        let catalog_id = match root_ref {
            Object::Reference(id) => *id,
            _ => return Err(AppError::Pdf("trailer /Root is not an indirect reference".into())),
        };

        let mut starts = vec![catalog_id];
        if let Ok(info_ref) = src.trailer.get(b"Info") {
            if let Object::Reference(id) = info_ref {
                starts.push(*id);
            }
        }
        let mut seen: HashSet<ObjectId> = HashSet::new();
        let mut closure: Vec<ObjectId> = Vec::new();
        for &start in &starts {
            Self::expand_closure(&src, start, true, &mut seen, &mut closure);
        }

        let mut out = Document::with_version(src.version.clone());
        let mut map: HashMap<ObjectId, ObjectId> = HashMap::new();
        for &id in &closure {
            map.insert(id, out.new_object_id());
        }
        for &id in &closure {
            let obj = src.objects.get(&id).cloned().unwrap_or(Object::Null);
            let copied = Self::remap_object(&obj, &src, &mut map, &mut out);
            out.objects.insert(map[&id], copied);
        }

        out.trailer.set("Root", map[&catalog_id]);
        if let Ok(info_ref) = src.trailer.get(b"Info") {
            if let Object::Reference(id) = info_ref {
                if let Some(&new_id) = map.get(&id) {
                    out.trailer.set("Info", new_id);
                }
            }
        }
        if let Ok(id_value) = src.trailer.get(b"ID") {
            out.trailer.set("ID", id_value.clone());
        }

        for value in out.objects.values_mut() {
            if let Object::Stream(stream) = value {
                // Skip streams that are already filtered; `compress` only
                // shrinks when the result is actually smaller.
                if stream.dict.get(b"Filter").is_err() {
                    let _ = stream.compress();
                }
            }
        }

        let pages = src.get_pages().len() as u32;
        let written = self.save_pdf(&mut out, output)?;
        Ok(OperationReport {
            status: "success",
            operation: "compress",
            output_file: display_path(&written),
            pages,
        })
    }

    /// Read-only document report: page count, byte size, encryption state,
    /// metadata and object-table size. No file is written.
    pub fn info(&self, input: &Path) -> AppResult<InfoReport> {
        let resolved = self.resolve(input)?;
        let doc = Document::load(&resolved).map_err(|e| AppError::Pdf(e.to_string()))?;
        let file_size_bytes = std::fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);

        let mut title: Option<String> = None;
        let mut creator: Option<String> = None;
        if let Ok(info_ref) = doc.trailer.get(b"Info") {
            if let Object::Reference(id) = info_ref {
                if let Ok(d) = doc.get_dictionary(*id) {
                    if let Some(t) = Self::dict_text(d.get(b"Title").ok()) {
                        title = Some(t);
                    }
                    if let Some(c) = Self::dict_text(d.get(b"Creator").ok()) {
                        creator = Some(c);
                    }
                }
            }
        }

        Ok(InfoReport {
            status: "success",
            operation: "info",
            source_file: display_path(&resolved),
            pages: doc.get_pages().len() as u32,
            file_size_bytes,
            object_count: doc.objects.len(),
            encrypted: doc.is_encrypted(),
            title,
            creator,
        })
    }

    /// Decode a metadata value, which may be a PDF `Name` or a literal
    /// `String`, into lossy UTF-8.
    fn dict_text(obj: Option<&Object>) -> Option<String> {
        let s = match obj? {
            Object::Name(bytes) => bytes,
            Object::String(bytes, _) => bytes,
            _ => return None,
        };
        Some(String::from_utf8_lossy(s).into_owned())
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

    /// Validate `ranges` against the document and return the selected page
    /// object ids in document order, de-duplicated.
    fn select_pages(src: &Document, ranges: &[Range]) -> AppResult<Vec<ObjectId>> {
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
        Ok(selected)
    }

    /// Build a fresh document containing exactly `ordered_pages` (in that
    /// order).
    ///
    /// Copies every object reachable from those pages — excluding `/Parent`
    /// references, so no unselected sibling page ever lands in the output —
    /// rewrites each page's `/Parent` to the new root, carries the source
    /// root's `/MediaBox`, and attaches a new catalog + trailer.
    fn build_extracted_document(
        src: &Document,
        ordered_pages: &[ObjectId],
    ) -> AppResult<Document> {
        let mut out = Document::with_version("1.7");
        let root_pages = out.new_object_id();

        let mut seen: HashSet<ObjectId> = HashSet::new();
        let mut closure: Vec<ObjectId> = Vec::new();
        for &page in ordered_pages {
            Self::expand_closure(src, page, false, &mut seen, &mut closure);
        }
        let mut map: HashMap<ObjectId, ObjectId> = HashMap::new();
        for &id in &closure {
            map.insert(id, out.new_object_id());
        }
        for &id in &closure {
            let obj = src.objects.get(&id).cloned().unwrap_or(Object::Null);
            let copied = Self::remap_object(&obj, src, &mut map, &mut out);
            out.objects.insert(map[&id], copied);
        }
        for &page in ordered_pages {
            if let Some(Object::Dictionary(d)) = out.objects.get_mut(&map[&page]) {
                d.set("Parent", root_pages);
            }
        }

        // Carry the media box from the source root when present so viewers
        // fall back to the document's own page size instead of a default.
        let media_box = Self::root_pages_id(src)
            .ok()
            .and_then(|id| src.get_dictionary(id).ok())
            .and_then(|d| d.get(b"MediaBox").ok())
            .cloned();
        let mut root_dict = dictionary! {
            "Type" => "Pages",
            "Kids" => Object::Array(ordered_pages.iter().map(|id| Object::Reference(map[id])).collect()),
            "Count" => Object::Integer(ordered_pages.len() as i64),
        };
        if let Some(box_obj) = media_box {
            root_dict.set("MediaBox", box_obj);
        }
        out.objects.insert(root_pages, Object::Dictionary(root_dict));

        let catalog_id = out.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => root_pages,
        });
        out.trailer.set("Root", catalog_id);
        Ok(out)
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

/// Windows `canonicalize` yields verbatim `\\?\...` paths. They are valid,
/// but ugly in a JSON payload — strip the prefix for report fields.
fn display_path(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(name: &str) -> PathBuf {
        // Thread-scoped dirs keep parallel tests from racing on the same files.
        let current = std::thread::current();
        // Test threads are named like `engine::tests::merge_...`; `::` is not
        // a legal filename character on Windows, so keep just the function part.
        let thread = current
            .name()
            .unwrap_or("main")
            .rsplit("::")
            .next()
            .unwrap_or("main");
        let dir = std::env::temp_dir().join(format!(
            "localpdf-tui-tests-{name}-{thread}-{}",
            std::process::id()
        ));
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

    /// A merged three-page document: page N's content contains "Sample N".
    fn three_page_doc() -> (Engine, PathBuf) {
        let (engine, files) = three_samples();
        engine.merge(&files, Path::new("three.pdf")).unwrap();
        let src = engine.root().join("three.pdf");
        (engine, src)
    }

    fn page_contains(doc: &Document, page_id: ObjectId, needle: &[u8]) -> bool {
        let content = doc.get_page_content(page_id);
        content.windows(needle.len()).any(|w| w == needle)
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

    #[test]
    fn rotate_sets_page_rotate() {
        let (engine, src) = three_page_doc();
        let report = engine.rotate(&src, "1-3", 90, Path::new("rot.pdf")).unwrap();
        assert_eq!(report.pages, 3);
        let loaded = Document::load(engine.root().join("rot.pdf")).unwrap();
        for (_n, page_id) in loaded.get_pages() {
            let rot = loaded.get_dictionary(page_id).unwrap().get(b"Rotate").unwrap();
            match rot {
                Object::Integer(deg) => assert_eq!(*deg, 90),
                other => panic!("expected /Rotate integer, got {other:?}"),
            }
        }
        // bad angle is rejected before touching the file
        let err = engine.rotate(&src, "1", 45, Path::new("bad.pdf")).unwrap_err();
        assert!(matches!(err, AppError::PageRange(_)));
    }

    #[test]
    fn reorder_changes_page_order() {
        let (engine, src) = three_page_doc();
        let report = engine.reorder(&src, "3,1,2", Path::new("reordered.pdf")).unwrap();
        assert_eq!(report.pages, 3);
        let loaded = Document::load(engine.root().join("reordered.pdf")).unwrap();
        let ids: Vec<ObjectId> = loaded.get_pages().into_values().collect();
        assert_eq!(ids.len(), 3);
        assert!(page_contains(&loaded, ids[0], b"Sample 3"));
        assert!(page_contains(&loaded, ids[1], b"Sample 1"));
        assert!(page_contains(&loaded, ids[2], b"Sample 2"));
    }

    #[test]
    fn reorder_rejects_bad_orders() {
        let (engine, src) = three_page_doc();
        let err = engine.reorder(&src, "1,2", Path::new("x.pdf")).unwrap_err();
        assert!(matches!(err, AppError::Operation(_))); // incomplete list
        let err = engine.reorder(&src, "2,2,1", Path::new("x.pdf")).unwrap_err();
        assert!(matches!(err, AppError::Operation(_))); // duplicate
        let err = engine.reorder(&src, "4,1,2", Path::new("x.pdf")).unwrap_err();
        assert!(matches!(err, AppError::PageRange(_))); // out of range
    }

    #[test]
    fn delete_pages_keeps_complement() {
        let (engine, src) = three_page_doc();
        let report = engine.delete_pages(&src, "2", Path::new("trimmed.pdf")).unwrap();
        assert_eq!(report.pages, 2);
        let loaded = Document::load(engine.root().join("trimmed.pdf")).unwrap();
        let ids: Vec<ObjectId> = loaded.get_pages().into_values().collect();
        assert!(page_contains(&loaded, ids[0], b"Sample 1"));
        assert!(page_contains(&loaded, ids[1], b"Sample 3"));
        // deleting everything is refused
        let err = engine.delete_pages(&src, "1-3", Path::new("none.pdf")).unwrap_err();
        assert!(matches!(err, AppError::Operation(_)));
    }

    #[test]
    fn compress_produces_valid_doc() {
        let (engine, src) = three_page_doc();
        let report = engine.compress(&src, Path::new("compressed.pdf")).unwrap();
        assert_eq!(report.pages, 3);
        let loaded = Document::load(engine.root().join("compressed.pdf")).unwrap();
        assert_eq!(loaded.get_pages().len(), 3);
        let ids: Vec<ObjectId> = loaded.get_pages().into_values().collect();
        assert!(page_contains(&loaded, ids[2], b"Sample 3"));
    }

    #[test]
    fn info_reports_metadata() {
        let (engine, files) = three_samples();
        let r = engine.info(&files[0]).unwrap();
        assert_eq!(r.pages, 1);
        assert!(!r.encrypted);
        assert!(r.object_count > 3);
        assert_eq!(r.title.as_deref(), Some("Sample 1"));
        assert_eq!(r.creator.as_deref(), Some("LocalPDF-TUI"));
        assert!(r.file_size_bytes > 0);
    }
}
