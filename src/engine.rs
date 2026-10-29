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

/// Report for `--extract-text`: the decoded text plus where it went
/// (embedded in the JSON when no output file was requested).
#[derive(Debug, Serialize)]
pub struct ExtractTextReport {
    pub status: &'static str,
    pub operation: &'static str,
    pub source_file: String,
    pub pages: u32,
    pub characters: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Report for `--extract-images`: what was pulled out of the XObject table.
#[derive(Debug, Serialize)]
pub struct ExtractImagesReport {
    pub status: &'static str,
    pub operation: &'static str,
    pub source_file: String,
    pub found: u32,
    pub files: Vec<String>,
    pub skipped: u32,
    pub output_dir: String,
}

/// Report for `--encrypt` / `--decrypt`.
#[derive(Debug, Serialize)]
pub struct EncryptionReport {
    pub status: &'static str,
    pub operation: &'static str,
    pub source_file: String,
    pub output_file: String,
    pub pages: u32,
    pub encrypted: bool,
    pub algorithm: &'static str,
}

/// Report for `--repair`: a clean re-save of a damaged document.
#[derive(Debug, Serialize)]
pub struct RepairReport {
    pub status: &'static str,
    pub operation: &'static str,
    pub source_file: String,
    pub output_file: String,
    pub pages: u32,
    pub recovered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_kind: Option<&'static str>,
}

/// Per-file outcome of `--batch`.
#[derive(Debug, Serialize)]
pub struct BatchResult {
    pub file: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Report for `--batch`: every PDF in a directory run through one op.
#[derive(Debug, Serialize)]
pub struct BatchReport {
    pub status: &'static str,
    pub operation: &'static str,
    pub input_dir: String,
    pub output_dir: String,
    pub processed: u32,
    pub results: Vec<BatchResult>,
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

/// Result of trying to pull usable pixel data out of an image XObject.
enum ImagePayload {
    /// DCT-encoded (JPEG) — can be written to disk untouched.
    Jpeg(Vec<u8>),
    /// 8-bit raw pixels, decoded and ready for PNG re-encoding.
    Pixels {
        width: u32,
        height: u32,
        rgb: bool,
        bytes: Vec<u8>,
    },
    /// Something we refuse to guess at (CMYK, indexed, 16-bit, JPX...).
    Unsupported,
}

/// A decompression bomb in an image stream must not be able to exhaust
/// memory: cap the expanded pixel data.
const MAX_IMAGE_STREAM: usize = 64 * 1024 * 1024;

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
    /// every unfiltered stream that gets smaller, at the given level 0-9.
    pub fn compress(
        &self,
        input: &Path,
        output: &Path,
        level: u32,
    ) -> AppResult<OperationReport> {
        match level {
            0..=9 => {}
            _ => {
                return Err(AppError::Operation(format!(
                    "--level must be 0-9 (got {level})"
                )));
            }
        }
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
                // Skip streams that are already filtered; compression is
                // applied only when the result is actually smaller.
                if stream.dict.get(b"Filter").is_err() {
                    let compressed = zlib_compress_level(&stream.content, level)?;
                    if compressed.len() + 19 < stream.content.len() {
                        stream.dict.set("Filter", "FlateDecode");
                        stream.set_content(compressed);
                    }
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

    /// Decode the text layer of the given pages (`page_spec = None` means all
    /// of them). With an `out` path the text is written there; otherwise it is
    /// returned in the report so the caller can embed it.
    pub fn extract_text(
        &self,
        input: &Path,
        page_spec: Option<&str>,
        out: Option<&Path>,
    ) -> AppResult<ExtractTextReport> {
        let resolved = self.resolve(input)?;
        let doc = self.load_pdf(&resolved)?;
        let total = u32::try_from(doc.get_pages().len()).unwrap_or(0);
        if total == 0 {
            return Err(AppError::Operation("document contains no pages".into()));
        }

        let nums: Vec<u32> = match page_spec {
            None => (1..=total).collect(),
            Some(spec) => {
                let ranges = parse_ranges(spec)?;
                let mut nums = Vec::new();
                for r in &ranges {
                    if r.start > total {
                        return Err(AppError::PageRange(format!(
                            "page {} is beyond the end of a {}-page document",
                            r.start, total
                        )));
                    }
                    nums.extend(r.start..=r.end.min(total));
                }
                nums
            }
        };

        // Bounded so a decompression-bomb content stream can't exhaust memory.
        const MAX_TEXT_BYTES: usize = 32 * 1024 * 1024;
        let text = doc
            .extract_text_with_limit(&nums, MAX_TEXT_BYTES)
            .map_err(|e| AppError::Operation(e.to_string()))?;

        let output_file = match out {
            Some(dest) => {
                let path = self.resolve_destination(dest)?;
                std::fs::write(&path, text.as_bytes())
                    .map_err(|e| AppError::Io("write text".to_string(), e))?;
                Some(display_path(&path))
            }
            None => None,
        };
        // Embed the text in the JSON only when it went nowhere else.
        let embed_text = output_file.is_none();

        Ok(ExtractTextReport {
            status: "success",
            operation: "extract-text",
            source_file: display_path(&resolved),
            pages: nums.len() as u32,
            characters: text.chars().count() as u32,
            output_file,
            text: embed_text.then_some(text),
        })
    }

    /// Pull raster images out of the document's XObject table into
    /// `out_dir` (created if missing). JPEG streams are copied through
    /// untouched; raw/Flate-decoded pixel data is re-encoded as PNG.
    /// Unsupported encodings (CMYK, indexed, 16-bit) count as skipped.
    pub fn extract_images(
        &self,
        input: &Path,
        out_dir: &Path,
    ) -> AppResult<ExtractImagesReport> {
        let resolved = self.resolve(input)?;
        let doc = Document::load(&resolved).map_err(|e| AppError::Pdf(e.to_string()))?;
        let dir = self.resolve_destination(out_dir)?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| AppError::Io("create output dir".to_string(), e))?;

        // Page number -> (resource name -> xobject id), so output files can be
        // named after the page that carries the image.
        let mut located: BTreeMap<ObjectId, (u32, String)> = BTreeMap::new();
        for (page_num, page_id) in doc.get_pages() {
            let page = match doc.objects.get(&page_id) {
                Some(Object::Dictionary(d)) => d,
                _ => continue,
            };
            let xobjs = Self::page_xobject_table(&doc, page);
            for (name, id) in xobjs {
                located.entry(id).or_insert_with(|| (page_num, name));
            }
        }

        let mut files: Vec<String> = Vec::new();
        let mut skipped: u32 = 0;
        for (id, obj) in doc.objects.iter() {
            let stream = match obj {
                Object::Stream(s) if Self::stream_is_image(s) => s,
                _ => continue,
            };
            let (page_num, name) = located
                .get(id)
                .cloned()
                .unwrap_or((0, String::from("x")));
            match Self::decode_image_stream(stream, &name) {
                ImagePayload::Jpeg(bytes) => {
                    let file_name = format!("page{page_num}-{name}.jpg");
                    std::fs::write(dir.join(&file_name), &bytes)
                        .map_err(|e| AppError::Io("write image".to_string(), e))?;
                    files.push(file_name);
                }
                ImagePayload::Pixels {
                    width,
                    height,
                    rgb,
                    bytes,
                } => {
                    let file_name = format!("page{page_num}-{name}.png");
                    let img = if rgb {
                        let pixels = image::RgbImage::from_raw(width, height, bytes)
                            .ok_or_else(|| AppError::Operation("bad RGB pixel data".into()))?;
                        image::DynamicImage::ImageRgb8(pixels)
                    } else {
                        let pixels = image::GrayImage::from_raw(width, height, bytes)
                            .ok_or_else(|| AppError::Operation("bad gray pixel data".into()))?;
                        image::DynamicImage::ImageLuma8(pixels)
                    };
                    img.save(dir.join(&file_name))
                        .map_err(|e| AppError::Operation(format!("png encode: {e}")))?;
                    files.push(file_name);
                }
                ImagePayload::Unsupported => skipped += 1,
            }
        }

        Ok(ExtractImagesReport {
            status: "success",
            operation: "extract-images",
            source_file: display_path(&resolved),
            found: files.len() as u32 + skipped,
            files,
            skipped,
            output_dir: display_path(&dir),
        })
    }

    /// `true` when the stream is an image XObject.
    fn stream_is_image(stream: &Stream) -> bool {
        stream
            .dict
            .get(b"Subtype")
            .ok()
            .and_then(|o| o.as_name().ok())
            == Some(b"Image")
    }

    fn decode_image_stream(stream: &Stream, _name: &str) -> ImagePayload {
        let dict = &stream.dict;
        let filter = dict.get(b"Filter").ok().and_then(|o| o.as_name().ok());
        let raw = match filter {
            Some(b"DCTDecode") => return ImagePayload::Jpeg(stream.content.clone()),
            Some(b"JPXDecode") | Some(b"JBIG2Decode") => return ImagePayload::Unsupported,
            Some(b"FlateDecode") => {
                let mut out = Vec::new();
                let mut decoder = flate2::read::ZlibDecoder::new(stream.content.as_slice());
                let mut buf = [0u8; 32 * 1024];
                loop {
                    match std::io::Read::read(&mut decoder, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            out.extend_from_slice(&buf[..n]);
                            if out.len() > MAX_IMAGE_STREAM {
                                return ImagePayload::Unsupported;
                            }
                        }
                        Err(_) => return ImagePayload::Unsupported,
                    }
                }
                out
            }
            Some(_) => return ImagePayload::Unsupported,
            None => stream.content.clone(),
        };

        let channels = match dict.get(b"ColorSpace").ok().and_then(|o| o.as_name().ok()) {
            Some(b"DeviceGray") => 1,
            Some(b"DeviceRGB") => 3,
            _ => return ImagePayload::Unsupported,
        };
        let width = dict.get(b"Width").ok().and_then(|o| o.as_i64().ok());
        let height = dict.get(b"Height").ok().and_then(|o| o.as_i64().ok());
        let bpc = dict
            .get(b"BitsPerComponent")
            .ok()
            .and_then(|o| o.as_i64().ok())
            .unwrap_or(8);
        let (w, h) = match (width, height) {
            (Some(w), Some(h)) if w > 0 && h > 0 && bpc == 8 => (w as u32, h as u32),
            _ => return ImagePayload::Unsupported,
        };
        let expected = w as usize * h as usize * channels as usize;
        if raw.len() < expected {
            return ImagePayload::Unsupported;
        }
        let bytes = raw[..expected].to_vec();
        ImagePayload::Pixels {
            width: w,
            height: h,
            rgb: channels == 3,
            bytes,
        }
    }

    /// Resolve a page's `/Resources << /XObject ... >>` table into
    /// `(resource name, object id)` pairs, following references.
    fn page_xobject_table(doc: &Document, page: &Dictionary) -> Vec<(String, ObjectId)> {
        let resources = match page.get(b"Resources").ok().and_then(|o| match o {
            Object::Reference(id) => doc.objects.get(&id).cloned(),
            other => Some(other.clone()),
        }) {
            Some(Object::Dictionary(d)) => d,
            _ => return Vec::new(),
        };
        let xobjects = match resources.get(b"XObject").ok().and_then(|o| match o {
            Object::Reference(id) => doc.objects.get(&id).cloned(),
            other => Some(other.clone()),
        }) {
            Some(Object::Dictionary(d)) => d,
            _ => return Vec::new(),
        };
        xobjects
            .iter()
            .filter_map(|(k, v)| {
                let id = match v {
                    Object::Reference(id) => *id,
                    _ => return None,
                };
                Some((String::from_utf8_lossy(k).into_owned(), id))
            })
            .collect()
    }

    /// Lay a centered, diagonal, semi-transparent text watermark on every
    /// page. One shared content stream is appended to each page's
    /// `/Contents`, so the source pages' content is preserved byte-for-byte.
    pub fn watermark(&self, input: &Path, text: &str, output: &Path) -> AppResult<OperationReport> {
        let text = text.trim();
        if text.is_empty() || text.len() > 120 {
            return Err(AppError::Operation(
                "watermark text must be 1-120 characters".into(),
            ));
        }
        let resolved = self.resolve(input)?;
        let mut doc = Document::load(&resolved).map_err(|e| AppError::Pdf(e.to_string()))?;

        let font = Self::font_object();
        let font_id = doc.add_object(Object::Dictionary(font));
        let overlay = Self::overlay_stream(&font_id, &format!(
            "q 0.35355 0.35355 -0.35355 0.35355 120 320 cm 0.5 g /F1 48 Tf ({}) Tj Q",
            escape_pdf_string(text)
        ));
        let overlay_id = doc.add_object(Object::Stream(overlay));
        Self::append_contents(&mut doc, overlay_id)?;

        let written = self.save_pdf(&mut doc, output)?;
        Ok(OperationReport {
            status: "success",
            operation: "watermark",
            output_file: display_path(&written),
            pages: doc.get_pages().len() as u32,
        })
    }

    /// Print the 1-based page number, centered in the bottom margin, of every
    /// page. Uses the same shared-overlay mechanism as [`Self::watermark`].
    pub fn page_numbers(&self, input: &Path, output: &Path) -> AppResult<OperationReport> {
        let resolved = self.resolve(input)?;
        let mut doc = Document::load(&resolved).map_err(|e| AppError::Pdf(e.to_string()))?;

        let font = Self::font_object();
        let font_id = doc.add_object(Object::Dictionary(font));
        // One overlay per page, each appended only to its own page (the
        // shared-overlay `append_contents` helper would stamp every page).
        let page_list: Vec<(u32, ObjectId)> = doc.get_pages().into_iter().collect();
        let overlays: Vec<ObjectId> = page_list
            .iter()
            .map(|(page_num, _)| {
                doc.add_object(Object::Stream(Self::overlay_stream(
                    &font_id,
                    &format!("q 0.2 g /F1 10 Tf 260 20 Td ({page_num}) Tj Q"),
                )))
            })
            .collect();
        for ((_num, page_id), overlay_id) in page_list.iter().zip(overlays.iter()) {
            Self::append_content_to_page(&mut doc, *page_id, *overlay_id)?;
        }
        let written = self.save_pdf(&mut doc, output)?;
        Ok(OperationReport {
            status: "success",
            operation: "page-numbers",
            output_file: display_path(&written),
            pages: doc.get_pages().len() as u32,
        })
    }

    /// Set arbitrary `Key=Value` pairs on the document's Info dictionary.
    /// The dict is created if the document has none.
    pub fn set_metadata(
        &self,
        input: &Path,
        pairs: &[(String, String)],
        output: &Path,
    ) -> AppResult<OperationReport> {
        if pairs.is_empty() {
            return Err(AppError::Operation("no metadata pairs supplied".into()));
        }
        let resolved = self.resolve(input)?;
        let mut doc = Document::load(&resolved).map_err(|e| AppError::Pdf(e.to_string()))?;

        let apply = |d: &mut Dictionary| {
            for (k, v) in pairs {
                d.set(k.as_str(), Object::string_literal(v.as_bytes()));
            }
        };
        match doc.trailer.get(b"Info").ok() {
            Some(Object::Reference(id)) => {
                let dict = doc
                    .get_dictionary_mut(*id)
                    .map_err(|e| AppError::Pdf(e.to_string()))?;
                apply(dict);
            }
            _ => {
                let mut dict = Dictionary::new();
                apply(&mut dict);
                let info_id = doc.add_object(Object::Dictionary(dict));
                doc.trailer.set("Info", Object::Reference(info_id));
            }
        }

        let written = self.save_pdf(&mut doc, output)?;
        Ok(OperationReport {
            status: "success",
            operation: "metadata",
            output_file: display_path(&written),
            pages: doc.get_pages().len() as u32,
        })
    }

    /// Assemble a new document from raster images — one page per image,
    /// sized to the image's pixel dimensions. JPEGs are embedded with
    /// `DCTDecode` (no re-encode); everything else is decoded via the
    /// `image` crate and stored as raw pixels.
    pub fn from_images(&self, inputs: &[&Path], output: &Path) -> AppResult<OperationReport> {
        if inputs.is_empty() {
            return Err(AppError::Operation("no images supplied".into()));
        }
        let mut doc = Document::new();
        let mut kids: Vec<Object> = Vec::new();
        let mut page_ids: Vec<ObjectId> = Vec::new();

        for image_path in inputs {
            let resolved = self.resolve(image_path)?;
            let bytes = std::fs::read(&resolved)
                .map_err(|e| AppError::Io("read image".to_string(), e))?;
            let image_stream = lopdf::xobject::image_from(bytes)
                .map_err(|e| AppError::Operation(format!("cannot embed {resolved:?}: {e}")))?;
            let image_id = doc.add_object(Object::Stream(image_stream));
            let stream_dict = match &doc.objects[&image_id] {
                Object::Stream(s) => &s.dict,
                _ => return Err(AppError::Operation("embedded image stream missing".into())),
            };
            let width = stream_dict.get(b"Width").ok().and_then(|o| o.as_i64().ok()).unwrap_or(1);
            let height = stream_dict.get(b"Height").ok().and_then(|o| o.as_i64().ok()).unwrap_or(1);

            let resources = Self::xobject_resources(&image_id);
            let resources_id = doc.add_object(Object::Dictionary(resources));
            let content =
                format!("q {width} 0 0 {height} 0 0 cm /Im0 Do Q").into_bytes();
            let content_id = doc.add_object(Object::Stream(Stream::new(Dictionary::new(), content)));

            let mut page = Dictionary::new();
            page.set("Type", Object::Name(b"Page".to_vec()));
            page.set(
                "MediaBox",
                Object::Array(
                    [0i64, 0, width, height]
                        .iter()
                        .map(|v| Object::Integer(*v))
                        .collect(),
                ),
            );
            page.set("Contents", Object::Reference(content_id));
            page.set("Resources", Object::Reference(resources_id));
            let page_id = doc.add_object(Object::Dictionary(page));
            kids.push(Object::Reference(page_id));
            page_ids.push(page_id);
        }

        let page_count = kids.len() as u32;
        let mut pages = Dictionary::new();
        pages.set("Type", Object::Name(b"Pages".to_vec()));
        pages.set("Count", Object::Integer(page_count as i64));
        pages.set("Kids", Object::Array(kids));
        let pages_id = doc.add_object(Object::Dictionary(pages));

        let mut catalog = Dictionary::new();
        catalog.set("Type", Object::Name(b"Catalog".to_vec()));
        catalog.set("Pages", Object::Reference(pages_id));
        let catalog_id = doc.add_object(Object::Dictionary(catalog));
        doc.trailer.set("Root", Object::Reference(catalog_id));

        // Backfill /Parent links now that both node ids exist.
        if let Some(Object::Dictionary(d)) = doc.objects.get_mut(&pages_id) {
            d.set("Parent", Object::Reference(catalog_id));
        }
        for page_id in &page_ids {
            if let Some(Object::Dictionary(d)) = doc.objects.get_mut(page_id) {
                d.set("Parent", Object::Reference(pages_id));
            }
        }

        let written = self.save_pdf(&mut doc, output)?;
        Ok(OperationReport {
            status: "success",
            operation: "from-images",
            output_file: display_path(&written),
            pages: page_count,
        })
    }

    // ---- M3: encryption, repair, batch ---------------------------------

    /// Encrypt the document with AES-128 (revision-4 crypt filter). The
    /// owner password controls permissions; the user password opens the
    /// document.
    pub fn encrypt(
        &self,
        input: &Path,
        user_password: &str,
        owner_password: &str,
        output: &Path,
    ) -> AppResult<EncryptionReport> {
        let resolved = self.resolve(input)?;
        let mut doc = self.load_pdf(&resolved)?;
        if doc.is_encrypted() {
            return Err(AppError::Operation("document is already encrypted".into()));
        }
        // Revision 3/4 key derivation hashes the trailer /ID array, which
        // must exist — generate one when the source document lacks it.
        if doc
            .trailer
            .get(b"ID")
            .ok()
            .and_then(|o| o.as_array().ok())
            .is_none()
        {
            let id = Object::string_literal(make_file_id(&resolved));
            doc.trailer
                .set("ID", Object::Array(vec![id.clone(), id]));
        }
        let filter: std::sync::Arc<dyn lopdf::encryption::crypt_filters::CryptFilter> =
            std::sync::Arc::new(lopdf::encryption::crypt_filters::Aes128CryptFilter);
        let version = lopdf::EncryptionVersion::V4 {
            document: &doc,
            encrypt_metadata: true,
            crypt_filters: std::collections::BTreeMap::from([(b"StdCF".to_vec(), filter)]),
            stream_filter: b"StdCF".to_vec(),
            string_filter: b"StdCF".to_vec(),
            owner_password,
            user_password,
            permissions: lopdf::Permissions::all(),
        };
        let state = lopdf::EncryptionState::try_from(version)
            .map_err(|e| AppError::Operation(format!("encryption setup failed: {e}")))?;
        doc.encrypt(&state)
            .map_err(|e| AppError::Operation(format!("encrypt: {e}")))?;
        let written = self.save_pdf(&mut doc, output)?;
        Ok(EncryptionReport {
            status: "success",
            operation: "encrypt",
            source_file: display_path(&resolved),
            output_file: display_path(&written),
            pages: doc.get_pages().len() as u32,
            encrypted: true,
            algorithm: "AES-128",
        })
    }

    /// Decrypt an encrypted document back to a plain one. The file keeps
    /// every object; only the encryption layer is stripped.
    pub fn decrypt(
        &self,
        input: &Path,
        password: &str,
        output: &Path,
    ) -> AppResult<EncryptionReport> {
        let resolved = self.resolve(input)?;
        // Loading WITH the password materializes the object map and decrypts
        // it in place. A plain load of an encrypted file leaves a sparse
        // still-encrypted map, and saving that would corrupt the document.
        let mut doc = Document::load_with_password(&resolved, password)
            .map_err(|e| AppError::Operation(format!("decryption failed (wrong password?): {e}")))?;
        let written = self.save_pdf(&mut doc, output)?;
        Ok(EncryptionReport {
            status: "success",
            operation: "decrypt",
            source_file: display_path(&resolved),
            output_file: display_path(&written),
            pages: doc.get_pages().len() as u32,
            encrypted: false,
            algorithm: "none",
        })
    }

    /// Load a possibly-damaged document leniently (lopdf reconstructs broken
    /// cross-reference tables on load) and write a clean, re-saved copy.
    pub fn repair(&self, input: &Path, output: &Path) -> AppResult<RepairReport> {
        let resolved = self.resolve(input)?;
        let mut doc = Document::load(&resolved).map_err(|e| AppError::Pdf(e.to_string()))?;
        let pages = doc.get_pages().len() as u32;
        let written = self.save_pdf(&mut doc, output)?;
        let recovery_kind = match doc.xref_recovery {
            Some(lopdf::xref::XrefRecovery::OffsetCorrected) => Some("offset-corrected"),
            Some(lopdf::xref::XrefRecovery::Reconstructed) => Some("reconstructed"),
            _ => None,
        };
        Ok(RepairReport {
            status: "success",
            operation: "repair",
            source_file: display_path(&resolved),
            output_file: display_path(&written),
            pages,
            recovered: recovery_kind.is_some(),
            recovery_kind,
        })
    }

    /// Run one operation (`compress` | `repair` | `pagenum`) over every
    /// `*.pdf` in `input_dir`, writing results into `out_dir`. File order
    /// is sorted by name so the JSON report is deterministic.
    pub fn batch(&self, input_dir: &Path, op_name: &str, out_dir: &Path) -> AppResult<BatchReport> {
        match op_name {
            "compress" | "repair" | "pagenum" => {}
            other => {
                return Err(AppError::Operation(format!(
                    "batch op '{other}' is not supported (use compress, repair, or pagenum)"
                )));
            }
        }
        let in_dir = self.resolve(input_dir)?;
        let out_res = self.resolve_destination(out_dir)?;
        std::fs::create_dir_all(&out_res)
            .map_err(|e| AppError::Io("create batch output dir".to_string(), e))?;

        let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(&in_dir)
            .map_err(|e| AppError::Io("read batch input dir".to_string(), e))?
            .filter_map(|e| e.ok())
            .collect();
        entries.sort_by_key(|e| e.file_name());

        let mut results: Vec<BatchResult> = Vec::new();
        let mut processed = 0u32;
        for entry in &entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.to_lowercase().ends_with(".pdf") {
                continue;
            }
            let source = entry.path();
            let dest = out_res.join(&name);
            // Batch ops re-save documents; an encrypted source would need a
            // password the batch contract does not carry, so refuse it up
            // front instead of writing a broken file.
            if let Ok(probe) = Document::load(&source)
                && probe.is_encrypted()
            {
                results.push(BatchResult {
                    file: name,
                    status: "error",
                    output_file: None,
                    error: Some("encrypted file: decrypt it first, then batch".into()),
                });
                continue;
            }
            let run = match op_name {
                "compress" => self.compress(&source, &dest, 9).map(|_| ()),
                "repair" => self.repair(&source, &dest).map(|_| ()),
                "pagenum" => self.page_numbers(&source, &dest).map(|_| ()),
                _ => unreachable!("validated above"),
            };
            match run {
                Ok(()) => {
                    processed += 1;
                    results.push(BatchResult {
                        file: name,
                        status: "success",
                        output_file: Some(display_path(&dest)),
                        error: None,
                    });
                }
                Err(e) => results.push(BatchResult {
                    file: name,
                    status: "error",
                    output_file: None,
                    error: Some(e.to_string()),
                }),
            }
        }
        if results.is_empty() {
            return Err(AppError::Operation("no .pdf files in the input directory".into()));
        }

        Ok(BatchReport {
            status: "success",
            operation: "batch",
            input_dir: display_path(&in_dir),
            output_dir: display_path(&out_res),
            processed,
            results,
        })
    }

    // ---- small builders shared by watermark / page-numbers / from-images ----

    fn font_object() -> Dictionary {
        let mut dict = Dictionary::new();
        dict.set("BaseFont", Object::Name(b"Helvetica".to_vec()));
        dict.set("Subtype", Object::Name(b"Type1".to_vec()));
        dict.set("FontType", Object::Integer(3));
        dict
    }

    /// A page-content stream that draws `ops`, carrying its own Resources so
    /// it renders even when the page inherits none.
    fn overlay_stream(font_id: &ObjectId, ops: &str) -> Stream {
        let mut res = Dictionary::new();
        let mut fonts = Dictionary::new();
        fonts.set("F1", Object::Reference(*font_id));
        res.set("Font", Object::Dictionary(fonts));
        let mut dict = Dictionary::new();
        dict.set("Resources", Object::Dictionary(res));
        Stream::new(dict, ops.as_bytes().to_vec())
    }

    fn xobject_resources(image_id: &ObjectId) -> Dictionary {
        let mut xobjects = Dictionary::new();
        xobjects.set("Im0", Object::Reference(*image_id));
        let mut dict = Dictionary::new();
        dict.set("XObject", Object::Dictionary(xobjects));
        dict
    }

    /// Append a shared content stream to each page's `/Contents`, wrapping the
    /// existing content in an Array so nothing is lost.
    fn append_contents(doc: &mut Document, overlay_id: ObjectId) -> AppResult<()> {
        let page_ids: Vec<ObjectId> = doc.get_pages().into_iter().map(|(_, id)| id).collect();
        for page_id in &page_ids {
            Self::append_content_to_page(doc, *page_id, overlay_id)?;
        }
        Ok(())
    }

    /// Append one content stream to a single page's `/Contents`.
    fn append_content_to_page(doc: &mut Document, page_id: ObjectId, overlay_id: ObjectId) -> AppResult<()> {
        let page = doc
            .objects
            .get_mut(&page_id)
            .and_then(|o| match o {
                Object::Dictionary(d) => Some(d),
                _ => None,
            })
            .ok_or_else(|| AppError::Operation("page dictionary missing".into()))?;
        let existing = page
            .get(b"Contents")
            .ok()
            .cloned()
            .map(|c| match c {
                Object::Array(items) => items,
                other => vec![other],
            })
            .unwrap_or_default();
        let mut items = existing;
        items.push(Object::Reference(overlay_id));
        page.set("Contents", Object::Array(items));
        Ok(())
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

/// Zlib (RFC 1950) compression at level 0-9, used by `compress`.
fn zlib_compress_level(bytes: &[u8], level: u32) -> AppResult<Vec<u8>> {
    let mut encoder =
        flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(level));
    std::io::Write::write_all(&mut encoder, bytes)
        .map_err(|e| AppError::Operation(format!("zlib compress: {e}")))?;
    encoder
        .finish()
        .map_err(|e| AppError::Operation(format!("zlib finish: {e}")))
}

/// A pseudo-unique 16-hex-digit file identifier for the trailer /ID array.
fn make_file_id(source: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Hash::hash(
        &format!(
            "{}:{}:{}",
            source.display(),
            std::process::id(),
            now
        ),
        &mut hasher,
    );
    format!("{:016x}", hasher.finish())
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
        let report = engine.compress(&src, Path::new("compressed.pdf"), 9).unwrap();
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

    /// How many `/Contents` array entries a page carries (1 = plain stream).
    fn contents_refs(doc: &Document, page_id: ObjectId) -> usize {
        let page = doc.get_dictionary(page_id).unwrap();
        match page.get(b"Contents").unwrap() {
            Object::Array(items) => items.len(),
            _ => 1,
        }
    }

    #[test]
    fn watermark_appends_overlay_to_every_page() {
        let (engine, src) = three_page_doc();
        let report = engine.watermark(&src, "CONFIDENTIAL", Path::new("wm.pdf")).unwrap();
        assert_eq!(report.pages, 3);
        let doc = Document::load(engine.root().join("wm.pdf")).unwrap();
        for (_n, page_id) in doc.get_pages() {
            assert_eq!(contents_refs(&doc, page_id), 2);
        }
        let err = engine.watermark(&src, "", Path::new("bad.pdf")).unwrap_err();
        assert!(matches!(err, AppError::Operation(_)));
    }

    #[test]
    fn page_numbers_stamps_every_page() {
        let (engine, src) = three_page_doc();
        let report = engine.page_numbers(&src, Path::new("pn.pdf")).unwrap();
        assert_eq!(report.pages, 3);
        let doc = Document::load(engine.root().join("pn.pdf")).unwrap();
        for (_n, page_id) in doc.get_pages() {
            assert_eq!(contents_refs(&doc, page_id), 2);
        }
    }

    #[test]
    fn set_metadata_writes_info_dict() {
        let (engine, src) = three_page_doc();
        let pairs = vec![
            ("Title".to_string(), "Q3 Report".to_string()),
            ("Author".to_string(), "Yash".to_string()),
        ];
        engine.set_metadata(&src, &pairs, Path::new("meta.pdf")).unwrap();
        let doc = Document::load(engine.root().join("meta.pdf")).unwrap();
        let info_ref = doc.trailer.get(b"Info").unwrap();
        let info_id = match info_ref {
            Object::Reference(id) => *id,
            _ => panic!("Info should be an indirect reference"),
        };
        let d = doc.get_dictionary(info_id).unwrap();
        assert_eq!(d.get(b"Title").unwrap(), &Object::string_literal(b"Q3 Report"));
        assert!(engine.set_metadata(&src, &[], Path::new("x.pdf")).is_err());
    }

    #[test]
    fn extract_text_returns_page_text() {
        let (engine, src) = three_page_doc();
        let report = engine.extract_text(&src, None, None).unwrap();
        assert_eq!(report.pages, 3);
        let text = report.text.as_deref().unwrap();
        assert!(text.contains("Sample 1"), "text was: {text:?}");
        assert!(text.contains("Sample 3"), "text was: {text:?}");

        // A page spec narrows the extraction and writes to a file.
        let rep = engine
            .extract_text(&src, Some("2"), Some(Path::new("page2.txt")))
            .unwrap();
        assert_eq!(rep.pages, 1);
        assert!(rep.output_file.is_some());
        let body = std::fs::read_to_string(engine.root().join("page2.txt")).unwrap();
        assert!(body.contains("Sample 2"));
    }

    #[test]
    fn from_images_then_extract_images_roundtrip() {
        let (engine, _files) = three_samples();
        let ws = engine.root();
        let img_path = ws.join("tiny.png");
        {
            let img = image::DynamicImage::ImageRgb8(
                image::RgbImage::from_pixel(2, 2, image::Rgb([9, 8, 7])).into(),
            );
            img.save(&img_path).unwrap();
        }

        let report = engine.from_images(&[&img_path], Path::new("tiny.pdf")).unwrap();
        assert_eq!(report.pages, 1);

        let tiny = ws.join("tiny.pdf");
        let rep = engine.extract_images(&tiny, Path::new("tiny-images")).unwrap();
        assert_eq!(rep.found, 1);
        assert_eq!(rep.skipped, 0);
        assert_eq!(rep.files.len(), 1);
        let out_file = ws.join("tiny-images").join(&rep.files[0]);
        assert!(out_file.exists());
        let bytes = std::fs::read(&out_file).unwrap();
        let loaded = image::load_from_memory(&bytes).unwrap();
        assert_eq!(loaded.width(), 2);
        assert_eq!(loaded.height(), 2);
    }

    #[test]
    fn compress_rejects_bad_level() {
        let (engine, src) = three_page_doc();
        let err = engine.compress(&src, Path::new("c.pdf"), 10).unwrap_err();
        assert!(matches!(err, AppError::Operation(_)));
    }

    #[test]
    fn encrypt_then_decrypt_roundtrip() {
        let (engine, src) = three_page_doc();
        let enc_report = engine
            .encrypt(&src, "secret", "secret", Path::new("secret.pdf"))
            .unwrap();
        assert_eq!(enc_report.pages, 3);
        assert!(enc_report.encrypted);

        let enc_path = engine.root().join("secret.pdf");
        let loaded = Document::load(&enc_path).unwrap();
        assert!(loaded.is_encrypted());

        // Wrong password is rejected before a byte is written.
        let err = engine
            .decrypt(&enc_path, "nope", Path::new("x.pdf"))
            .unwrap_err();
        assert!(matches!(err, AppError::Operation(_)));

        let dec_report = engine
            .decrypt(&enc_path, "secret", Path::new("plain-again.pdf"))
            .unwrap();
        assert!(!dec_report.encrypted);
        let plain = Document::load(engine.root().join("plain-again.pdf")).unwrap();
        assert!(!plain.is_encrypted());
        assert_eq!(plain.get_pages().len(), 3);

        // Re-encrypting an encrypted document is refused.
        let err = engine
            .encrypt(&enc_path, "a", "b", Path::new("x.pdf"))
            .unwrap_err();
        assert!(matches!(err, AppError::Operation(_)));
    }

    #[test]
    fn repair_rescues_truncated_document() {
        let (engine, files) = three_samples();
        let bytes = std::fs::read(&files[0]).unwrap();
        // Chop the tail (drops %%EOF and part of the xref area).
        let damaged = &bytes[..bytes.len() - 6];
        let damaged_path = engine.root().join("damaged.pdf");
        std::fs::write(&damaged_path, damaged).unwrap();

        let report = engine
            .repair(&damaged_path, Path::new("repaired.pdf"))
            .unwrap();
        assert_eq!(report.pages, 1);
        let reloaded = Document::load(engine.root().join("repaired.pdf")).unwrap();
        assert_eq!(reloaded.get_pages().len(), 1);

        // Garbage that is not a PDF at all is refused.
        let garbage = engine.root().join("garbage.pdf");
        std::fs::write(&garbage, b"this is definitely not a pdf").unwrap();
        assert!(engine.repair(&garbage, Path::new("nope.pdf")).is_err());
    }

    #[test]
    fn batch_runs_op_over_every_pdf() {
        let (engine, files) = three_samples();
        let report = engine.batch(engine.root(), "compress", Path::new("batch-out")).unwrap();
        assert_eq!(report.processed, 3);
        assert_eq!(report.results.len(), 3);
        for r in &report.results {
            assert_eq!(r.status, "success", "{r:?}");
            let out = engine.root().join("batch-out").join(&r.file);
            assert!(out.exists(), "missing {out:?}");
        }
        // File order in the report is sorted by name.
        let names: Vec<&str> = report.results.iter().map(|r| r.file.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        let _ = &files;
    }

    #[test]
    fn batch_rejects_unknown_ops_and_empty_dirs() {
        let (engine, _files) = three_samples();
        let err = engine
            .batch(engine.root(), "merge", Path::new("o"))
            .unwrap_err();
        assert!(matches!(err, AppError::Operation(_)));

        let empty = engine.root().join("empty-dir");
        std::fs::create_dir_all(&empty).unwrap();
        let err = engine
            .batch(&empty, "compress", Path::new("o"))
            .unwrap_err();
        assert!(matches!(err, AppError::Operation(_)));
    }

    #[test]
    fn batch_flags_encrypted_files() {
        let (engine, files) = three_samples();
        engine
            .encrypt(&files[0], "pw", "pw", Path::new("secret-batch.pdf"))
            .unwrap();
        let report = engine
            .batch(engine.root(), "compress", Path::new("bo"))
            .unwrap();
        let flagged = report
            .results
            .iter()
            .find(|r| r.file == "secret-batch.pdf")
            .unwrap();
        assert_eq!(flagged.status, "error");
        assert!(flagged.error.is_some());
        // The other three files still processed successfully.
        assert_eq!(report.processed, 3);
    }
}
