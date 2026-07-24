//! File ingestion for the `ingest_file` Tier-7 tool.
//!
//! Port of the Python `cerebro.ingestion` package (adapters + pipeline),
//! folded into one module: the file is routed by extension to a format
//! handler, split into memory-sized chunks, and every chunk stored through
//! the normal `remember()` pipeline (thalamus gate → amygdala → …), so
//! ingested content behaves like any other memory.
//!
//! Format parity with the Python adapters, with two deliberate divergences:
//! - **Images** route through the tiered `cerebro::vision` backend
//!   (`describe` caption + CLIP `index_image`) instead of Python's
//!   Ollama-or-filename fallback — better captions, and the image lands in
//!   `search_vision`'s recall space. No OCR pass (the VLM caption covers
//!   visible text; tesseract would be a system dependency).
//! - **PDF** uses pure-Rust `lopdf` text extraction (Python needed the
//!   optional PyMuPDF). Embedded-image extraction is not ported.
//!
//! `session_id`/episode tracking is not advertised or accepted: the Rust
//! `remember()` has no session plumbing, and accepting-then-ignoring a
//! documented arg is exactly the C1 bug class the backport waves fixed.

use std::path::Path;
use std::time::Instant;

use anyhow::{anyhow, Result};
use serde::Serialize;

use crate::{
    cortex::CerebroCortex,
    types::{MemoryType, VisibilityScope},
};

/// Python TextAdapter.MAX_CHUNK_WORDS — a paragraph larger than this is split
/// at sentence boundaries into ≤500-word pieces.
const MAX_CHUNK_WORDS: usize = 500;
/// Python TextAdapter.MIN_CHUNK_LENGTH — shorter chunks are skipped (also the
/// thalamus MIN_CONTENT_LENGTH, so pre-skipping avoids per-chunk gate errors).
const MIN_CHUNK_LENGTH: usize = 10;
/// Python CSVAdapter.ROW_THRESHOLD — above this, store a schema summary
/// instead of one memory per row.
const CSV_ROW_THRESHOLD: usize = 200;

/// What the router decided a file is — reported back so the caller can see
/// which handler ran (and tests can assert routing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Text,
    Markdown,
    Json,
    Csv,
    Html,
    Pdf,
    Image,
}

/// Mirrors the Python `IngestionResult.to_dict()` shape (minus attachments,
/// which the Rust schema does not have).
#[derive(Debug, Default, Serialize)]
pub struct IngestReport {
    pub file: String,
    pub kind: Option<FileKind>,
    pub memories_imported: usize,
    pub memories_skipped: usize,
    pub memory_ids: Vec<String>,
    pub errors: Vec<String>,
    pub duration_secs: f64,
}

/// Extensions the plain-text handler accepts (Python TextAdapter.SUPPORTED,
/// minus the ones owned by a more specific handler: .md/.json route there).
const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "py", "js", "ts", "rs", "go", "java", "rb", "sh",
    "c", "cpp", "h", "hpp", "cs", "swift", "kt", "scala",
    "r", "m", "mm", "sql", "yaml", "yml", "xml",
    "toml", "ini", "cfg", "conf", "properties", "rst", "log",
];

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Route a file by extension. `None` = no handler for this type.
pub fn classify(path: &Path) -> Option<FileKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "md" | "markdown" | "mdown" | "mkd" => Some(FileKind::Markdown),
        "json" => Some(FileKind::Json),
        "csv" => Some(FileKind::Csv),
        "html" | "htm" => Some(FileKind::Html),
        "pdf" => Some(FileKind::Pdf),
        e if IMAGE_EXTENSIONS.contains(&e) => Some(FileKind::Image),
        e if TEXT_EXTENSIONS.contains(&e) => Some(FileKind::Text),
        _ => None,
    }
}

/// Ingest one file into memory. `extra_tags` are applied to every stored
/// memory; the file name is always stamped as a `source:<name>` tag so the
/// whole import can be found (and cleaned up) via `find_by_tags`.
pub async fn ingest_file(
    cortex: &CerebroCortex,
    path: &Path,
    extra_tags: Vec<String>,
    scope: VisibilityScope,
) -> Result<IngestReport> {
    let start = Instant::now();
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string());

    if !path.exists() {
        return Err(anyhow!("file not found: {}", path.display()));
    }
    if !path.is_file() {
        return Err(anyhow!("not a file: {}", path.display()));
    }
    let Some(kind) = classify(path) else {
        return Err(anyhow!(
            "no handler for file type '{}' (supported: text/code, md, json, csv, html, pdf, png/jpg/gif/webp)",
            path.extension().and_then(|e| e.to_str()).unwrap_or("<none>"),
        ));
    };

    let mut report = IngestReport {
        file: file_name.clone(),
        kind: Some(kind),
        ..Default::default()
    };

    // `source:<name>` on everything — the provenance/cleanup key.
    let mut base_tags = extra_tags;
    base_tags.push(format!("source:{file_name}"));

    match kind {
        FileKind::Text => {
            let text = read_text(path)?;
            store_chunks(cortex, chunk_text(&text), MemoryType::Semantic,
                &base_tags, &scope, &mut report).await;
        }
        FileKind::Html => {
            let text = strip_html(&read_text(path)?);
            store_chunks(cortex, chunk_text(&text), MemoryType::Semantic,
                &base_tags, &scope, &mut report).await;
        }
        FileKind::Markdown => {
            ingest_markdown(cortex, &read_text(path)?, base_tags.clone(), &scope, &mut report).await;
        }
        FileKind::Json => {
            ingest_json(cortex, &read_text(path)?, &base_tags, &scope, &mut report).await;
        }
        FileKind::Csv => {
            // Python's CSVAdapter tags `csv` + `file:<name>` (its own
            // convention) — kept for DB parity with Python-ingested rows.
            let mut csv_tags = base_tags.clone();
            csv_tags.push("csv".to_string());
            csv_tags.push(format!("file:{file_name}"));
            ingest_csv(cortex, &read_text(path)?, &csv_tags, &scope, &mut report).await;
        }
        FileKind::Pdf => {
            match extract_pdf_text(path) {
                Ok(text) => store_chunks(cortex, chunk_text(&text), MemoryType::Semantic,
                    &base_tags, &scope, &mut report).await,
                Err(e) => report.errors.push(format!("pdf extraction failed: {e}")),
            }
        }
        FileKind::Image => {
            ingest_image(cortex, path, base_tags.clone(), &scope, &mut report).await;
        }
    }

    report.duration_secs = start.elapsed().as_secs_f64();
    Ok(report)
}

fn read_text(path: &Path) -> Result<String> {
    // Python reads with errors="replace" — mirror: lossy, never fail on
    // stray bytes in an otherwise-text file.
    let bytes = std::fs::read(path)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Store a list of chunks through `remember`, tallying into the report.
async fn store_chunks(
    cortex: &CerebroCortex,
    chunks: Vec<String>,
    memory_type: MemoryType,
    tags: &[String],
    scope: &VisibilityScope,
    report: &mut IngestReport,
) {
    for chunk in chunks {
        let chunk = chunk.trim();
        if chunk.len() < MIN_CHUNK_LENGTH {
            report.memories_skipped += 1;
            continue;
        }
        match cortex.remember(
            chunk.to_string(),
            Some(memory_type),
            Some(tags.to_vec()),
            None,
            scope.clone(),
        ).await {
            Ok(node) => {
                report.memories_imported += 1;
                report.memory_ids.push(node.id.0);
            }
            Err(e) => {
                report.memories_skipped += 1;
                report.errors.push(format!("chunk rejected: {e}"));
            }
        }
    }
}

// ── chunking (Python TextAdapter._chunk_text) ───────────────────────────────

/// Split text into memory-sized chunks: blank-line paragraphs, with any
/// paragraph over `MAX_CHUNK_WORDS` re-split at sentence boundaries.
pub fn chunk_text(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    for para in split_paragraphs(text) {
        let words = para.split_whitespace().count();
        if words <= MAX_CHUNK_WORDS {
            chunks.push(para);
            continue;
        }
        // Pack sentences up to the word cap (Python's sentence re-split).
        let mut current: Vec<&str> = Vec::new();
        let mut current_words = 0usize;
        for sentence in split_sentences(&para) {
            let s_words = sentence.split_whitespace().count();
            if current_words + s_words > MAX_CHUNK_WORDS && !current.is_empty() {
                chunks.push(current.join(" "));
                current.clear();
                current_words = 0;
            }
            current.push(sentence);
            current_words += s_words;
        }
        if !current.is_empty() {
            chunks.push(current.join(" "));
        }
    }
    chunks
}

/// Split on blank lines (Python `re.split(r"\n\s*\n", text)`).
fn split_paragraphs(text: &str) -> Vec<String> {
    let mut paras = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !current.trim().is_empty() {
                paras.push(current.trim().to_string());
            }
            current.clear();
        } else {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(line);
        }
    }
    if !current.trim().is_empty() {
        paras.push(current.trim().to_string());
    }
    paras
}

/// Split after `.`/`!`/`?` followed by whitespace (Python
/// `re.split(r"(?<=[.!?])\s+", para)`). Returns borrowed slices.
fn split_sentences(para: &str) -> Vec<&str> {
    let bytes = para.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if matches!(bytes[i], b'.' | b'!' | b'?')
            && bytes.get(i + 1).is_some_and(|b| b.is_ascii_whitespace())
        {
            let end = i + 1;
            let sentence = para[start..end].trim();
            if !sentence.is_empty() {
                out.push(sentence);
            }
            // Skip the whitespace run to the next sentence start.
            let mut j = end;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            start = j;
            i = j;
        } else {
            i += 1;
        }
    }
    let tail = para[start..].trim();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

// ── markdown (Python MarkdownAdapter) ───────────────────────────────────────

async fn ingest_markdown(
    cortex: &CerebroCortex,
    text: &str,
    mut tags: Vec<String>,
    scope: &VisibilityScope,
    report: &mut IngestReport,
) {
    let (front, body) = parse_frontmatter(text);

    let default_type = front
        .iter()
        .find(|(k, _)| k == "type")
        .and_then(|(_, v)| serde_json::from_value::<MemoryType>(serde_json::json!(v)).ok())
        .unwrap_or(MemoryType::Semantic);
    for (k, v) in &front {
        if k == "tags" {
            tags.extend(v.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()));
        }
    }

    let sections = extract_sections(&body);
    if sections.is_empty() {
        // No headings → paragraph chunking, same tags.
        store_chunks(cortex, chunk_text(&body), default_type, &tags, scope, report).await;
        return;
    }

    for (title, content) in sections {
        let content = content.trim();
        if content.len() < 3 {
            report.memories_skipped += 1;
            continue;
        }
        let full = if title.is_empty() {
            content.to_string()
        } else {
            format!("{title}: {content}")
        };
        // Section title becomes a slug tag (Python: non-alnum → `_`).
        let mut sec_tags = tags.clone();
        if !title.is_empty() {
            let slug: String = title
                .to_lowercase()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect::<String>()
                .split('_')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("_");
            if !slug.is_empty() && !sec_tags.contains(&slug) {
                sec_tags.push(slug);
            }
        }
        store_chunks(cortex, vec![full], default_type, &sec_tags, scope, report).await;
    }
}

/// Parse simple `key: value` YAML frontmatter between `---` fences (Python's
/// PyYAML-free parser). Returns (pairs, body).
fn parse_frontmatter(text: &str) -> (Vec<(String, String)>, String) {
    let trimmed = text.trim_start();
    if !trimmed.starts_with("---") {
        return (Vec::new(), text.to_string());
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    let Some(end) = lines.iter().skip(1).position(|l| l.trim() == "---") else {
        return (Vec::new(), text.to_string());
    };
    let end = end + 1; // index in `lines`
    let mut pairs = Vec::new();
    for line in &lines[1..end] {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_string();
            let v = v.trim().trim_matches(|c| c == '[' || c == ']').to_string();
            if !k.is_empty() {
                pairs.push((k, v.trim_matches(|c| c == '\'' || c == '"').to_string()));
            }
        }
    }
    (pairs, lines[end + 1..].join("\n"))
}

/// `## Heading` sections (Python's `^##\s+(.+)$` multiline). Empty vec when
/// the document has no `##` headings.
fn extract_sections(text: &str) -> Vec<(String, String)> {
    // (heading line start, heading line end, title)
    let mut headings: Vec<(usize, usize, String)> = Vec::new();
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        if let Some(title) = line.trim_end().strip_prefix("## ") {
            headings.push((offset, offset + line.len(), title.trim().to_string()));
        }
        offset += line.len();
    }
    if headings.is_empty() {
        return Vec::new();
    }
    let mut sections = Vec::new();
    for (i, (_, body_start, title)) in headings.iter().enumerate() {
        let end = headings.get(i + 1).map_or(text.len(), |(next_start, _, _)| *next_start);
        let body = text[*body_start..end.max(*body_start)].trim().to_string();
        sections.push((title.clone(), body));
    }
    sections
}

// ── json (Python JSONAdapter) ───────────────────────────────────────────────

async fn ingest_json(
    cortex: &CerebroCortex,
    text: &str,
    base_tags: &[String],
    scope: &VisibilityScope,
    report: &mut IngestReport,
) {
    let data: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            report.errors.push(format!("invalid JSON: {e}"));
            return;
        }
    };
    // A list of records, or a {"memories":[...]}/{"records":[...]} wrapper.
    let records = match &data {
        serde_json::Value::Array(a) => a.clone(),
        serde_json::Value::Object(o) => o
            .get("memories")
            .or_else(|| o.get("records"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        _ => {
            report.errors.push("JSON root must be a list or dict".to_string());
            return;
        }
    };
    for (i, record) in records.iter().enumerate() {
        let (content, mem_type, rec_tags, salience) = match record {
            serde_json::Value::String(s) => (s.clone(), None, Vec::new(), None),
            serde_json::Value::Object(o) => {
                let content = o.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if content.len() < 3 {
                    report.memories_skipped += 1;
                    continue;
                }
                let mem_type: Option<MemoryType> = o
                    .get("type")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                let rec_tags: Vec<String> = o
                    .get("tags")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let salience = o.get("salience").and_then(|v| v.as_f64()).map(|f| f as f32);
                (content.to_string(), mem_type, rec_tags, salience)
            }
            _ => {
                report.errors.push(format!("record {i}: expected string or dict"));
                report.memories_skipped += 1;
                continue;
            }
        };
        let mut tags = base_tags.to_vec();
        tags.extend(rec_tags);
        match cortex.remember(content, mem_type, Some(tags), salience, scope.clone()).await {
            Ok(node) => {
                report.memories_imported += 1;
                report.memory_ids.push(node.id.0);
            }
            Err(e) => {
                report.memories_skipped += 1;
                report.errors.push(format!("record {i} rejected: {e}"));
            }
        }
    }
}

// ── csv (Python CSVAdapter) ─────────────────────────────────────────────────

async fn ingest_csv(
    cortex: &CerebroCortex,
    text: &str,
    tags: &[String],
    scope: &VisibilityScope,
    report: &mut IngestReport,
) {
    let delim = sniff_delimiter(text);
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let Some(header_line) = lines.next() else {
        report.errors.push("CSV file is empty".to_string());
        return;
    };
    let headers: Vec<&str> = header_line.split(delim).map(str::trim).collect();
    let rows: Vec<Vec<&str>> = lines
        .map(|l| l.split(delim).map(str::trim).collect())
        .collect();
    if rows.is_empty() {
        report.errors.push("CSV file is empty".to_string());
        return;
    }

    if rows.len() > CSV_ROW_THRESHOLD {
        // Schema-only mode: one summary memory instead of thousands of rows.
        let sample = rows[0]
            .iter()
            .zip(&headers)
            .map(|(v, h)| format!("{h}={v}"))
            .collect::<Vec<_>>()
            .join(", ");
        let summary = format!(
            "CSV dataset {}: {} rows, columns [{}]. Sample row: {}",
            report.file,
            rows.len(),
            headers.join(", "),
            sample,
        );
        let mut schema_tags = tags.to_vec();
        schema_tags.push("schema".to_string());
        store_chunks(cortex, vec![summary], MemoryType::Semantic, &schema_tags, scope, report).await;
        return;
    }

    for row in rows {
        let content = row
            .iter()
            .zip(&headers)
            .filter(|(v, _)| !v.is_empty())
            .map(|(v, h)| format!("{h}: {v}"))
            .collect::<Vec<_>>()
            .join(" | ");
        store_chunks(cortex, vec![content], MemoryType::Semantic, tags, scope, report).await;
    }
}

/// Pick the delimiter that splits the first line into the most fields —
/// a lightweight stand-in for Python's `csv.Sniffer`.
fn sniff_delimiter(text: &str) -> char {
    let first = text.lines().next().unwrap_or("");
    [',', ';', '\t', '|']
        .into_iter()
        .max_by_key(|d| first.matches(*d).count())
        .unwrap_or(',')
}

// ── html ────────────────────────────────────────────────────────────────────

/// Drop `<script>`/`<style>` blocks, strip tags, decode the common entities.
/// Deliberately naive (like Python's stdlib-only parser) — good enough to
/// make saved pages searchable, not a browser.
pub fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let lower = html.to_lowercase();
    let mut skip_until: Option<&str> = None;
    let mut i = 0usize;
    for (idx, c) in html.char_indices() {
        if idx < i {
            continue; // already consumed by a skip
        }
        if let Some(close) = skip_until {
            if let Some(pos) = lower[idx..].find(close) {
                i = idx + pos + close.len();
                skip_until = None;
            } else {
                break; // unterminated script/style — drop the rest
            }
            continue;
        }
        if c == '<' {
            if lower[idx..].starts_with("<script") {
                skip_until = Some("</script>");
                continue;
            }
            if lower[idx..].starts_with("<style") {
                skip_until = Some("</style>");
                continue;
            }
            // Block-level closes read better as paragraph breaks.
            if lower[idx..].starts_with("</p>") || lower[idx..].starts_with("</div>")
                || lower[idx..].starts_with("</h") || lower[idx..].starts_with("<br")
            {
                out.push('\n');
                out.push('\n');
            }
            if let Some(pos) = html[idx..].find('>') {
                i = idx + pos + 1;
            } else {
                break; // unterminated tag
            }
            continue;
        }
        out.push(c);
    }
    // The handful of entities that matter for readability.
    out.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

// ── pdf ─────────────────────────────────────────────────────────────────────

/// Extract all page text with lopdf. Basic (no layout reconstruction), but
/// pure Rust — Python needed the optional PyMuPDF for the same job.
fn extract_pdf_text(path: &Path) -> Result<String> {
    let doc = lopdf::Document::load(path)?;
    let pages: Vec<u32> = doc.get_pages().keys().cloned().collect();
    if pages.is_empty() {
        return Err(anyhow!("PDF has no pages"));
    }
    let text = doc.extract_text(&pages)?;
    if text.trim().is_empty() {
        return Err(anyhow!("no extractable text (scanned/image-only PDF?)"));
    }
    Ok(text)
}

// ── images ──────────────────────────────────────────────────────────────────

/// Caption via the tiered VLM backend, remember the caption (Episodic,
/// tagged `vision` like describe_image{remember} does), and best-effort
/// CLIP-index the image for search_vision.
async fn ingest_image(
    cortex: &CerebroCortex,
    path: &Path,
    mut tags: Vec<String>,
    scope: &VisibilityScope,
    report: &mut IngestReport,
) {
    let image = match crate::vision::prepare_from_path(&path.to_string_lossy()) {
        Ok(img) => img,
        Err(e) => {
            report.errors.push(format!("image read failed: {e}"));
            return;
        }
    };
    let cfg = crate::vision::VisionConfig::from_env();
    let caption = match crate::vision::describe(&cfg, &image, None).await {
        Ok(c) => c,
        Err(e) => {
            report.errors.push(format!("caption failed: {e}"));
            return;
        }
    };
    if !tags.iter().any(|t| t == "vision") {
        tags.push("vision".to_string());
    }
    match cortex.remember(
        caption.text,
        Some(MemoryType::Episodic),
        Some(tags),
        None,
        scope.clone(),
    ).await {
        Ok(node) => {
            report.memories_imported += 1;
            if let Ok(bytes) = image.decoded() {
                if let Err(e) = cortex
                    .index_image(&node.id, bytes, Some(path.to_string_lossy().to_string()))
                    .await
                {
                    report.errors.push(format!("visual index failed (caption kept): {e}"));
                }
            }
            report.memory_ids.push(node.id.0);
        }
        Err(e) => {
            report.memories_skipped += 1;
            report.errors.push(format!("caption rejected: {e}"));
        }
    }
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn classify_routes_by_extension() {
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(classify(&p("notes.md")), Some(FileKind::Markdown));
        assert_eq!(classify(&p("data.JSON")), Some(FileKind::Json));
        assert_eq!(classify(&p("t.csv")), Some(FileKind::Csv));
        assert_eq!(classify(&p("page.html")), Some(FileKind::Html));
        assert_eq!(classify(&p("doc.pdf")), Some(FileKind::Pdf));
        assert_eq!(classify(&p("shot.PNG")), Some(FileKind::Image));
        assert_eq!(classify(&p("main.rs")), Some(FileKind::Text));
        assert_eq!(classify(&p("binary.bin")), None);
        assert_eq!(classify(&p("no_extension")), None);
    }

    #[test]
    fn chunk_text_splits_paragraphs_and_long_ones_at_sentences() {
        // Two paragraphs → two chunks.
        let chunks = chunk_text("first paragraph here\n\nsecond paragraph here");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "first paragraph here");

        // One >500-word paragraph of sentences → multiple ≤500-word chunks.
        let sentence = "This sentence has exactly seven words in. ";
        let big = sentence.repeat(100); // 700 words, one paragraph
        let chunks = chunk_text(&big);
        assert!(chunks.len() >= 2, "must split, got {}", chunks.len());
        for c in &chunks {
            assert!(c.split_whitespace().count() <= MAX_CHUNK_WORDS,
                "chunk exceeds cap: {} words", c.split_whitespace().count());
        }
    }

    #[test]
    fn sentences_split_on_terminators() {
        let s = split_sentences("One. Two! Three? Four");
        assert_eq!(s, vec!["One.", "Two!", "Three?", "Four"]);
    }

    #[test]
    fn frontmatter_parses_and_strips() {
        let (front, body) = parse_frontmatter("---\ntype: procedural\ntags: [a, b]\n---\nBody text");
        assert!(front.iter().any(|(k, v)| k == "type" && v == "procedural"));
        assert!(front.iter().any(|(k, v)| k == "tags" && v == "a, b"));
        assert_eq!(body.trim(), "Body text");
        // No frontmatter → untouched.
        let (front, body) = parse_frontmatter("Just text");
        assert!(front.is_empty());
        assert_eq!(body, "Just text");
    }

    #[test]
    fn sections_extract_by_h2() {
        let md = "intro\n\n## First\nalpha beta\n\n## Second\ngamma delta\n";
        let sections = extract_sections(md);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].0, "First");
        assert!(sections[0].1.contains("alpha beta"));
        assert_eq!(sections[1].0, "Second");
        assert!(sections[1].1.contains("gamma delta"));
        // No ## headings → empty (caller falls back to paragraph chunking).
        assert!(extract_sections("plain\n\ntext").is_empty());
    }

    #[test]
    fn html_strips_tags_scripts_and_entities() {
        let html = "<html><head><style>body{color:red}</style>\
                    <script>var x=1;</script></head>\
                    <body><h1>Title</h1><p>Hello &amp; welcome</p></body></html>";
        let text = strip_html(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello & welcome"));
        assert!(!text.contains("var x"), "script content must be dropped");
        assert!(!text.contains("color:red"), "style content must be dropped");
        assert!(!text.contains('<'), "no tags may survive");
    }

    #[test]
    fn csv_delimiter_sniffing() {
        assert_eq!(sniff_delimiter("a,b,c\n1,2,3"), ',');
        assert_eq!(sniff_delimiter("a;b;c\n1;2;3"), ';');
        assert_eq!(sniff_delimiter("a\tb\tc"), '\t');
    }

    // Build a one-page PDF with lopdf, then extract it back — proves the
    // extraction path without any binary fixture in the repo.
    #[test]
    fn pdf_roundtrip_extracts_text() {
        use lopdf::content::{Content, Operation};
        use lopdf::{dictionary, Document, Object, Stream};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 24.into()]),
                Operation::new("Td", vec![100.into(), 600.into()]),
                Operation::new("Tj", vec![Object::string_literal("cerebro ingestion test page")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(
            dictionary! {}, content.encode().unwrap(),
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
        });
        doc.objects.insert(pages_id, Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        }));
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog", "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test.pdf");
        doc.save(&path).unwrap();

        let text = extract_pdf_text(&path).unwrap();
        assert!(text.contains("cerebro ingestion test page"),
            "extracted text must round-trip, got: {text:?}");
    }
}
