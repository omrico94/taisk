//! `MemoryRepo` — the repository boundary around LanceDB (see implementation
//! plan §2/§6). Keeps LanceDB's Arrow-based schema/query details out of the
//! rest of the engine behind a handful of narrow, purpose-built methods.
//!
//! Two tables, both created on first use (no migrations):
//! - `memories`: durable prompt/summary embeddings, backing both categorization
//!   lookups and historical semantic search (FR5+FR6).
//! - `category_exemplars`: one row per known category, used for the
//!   nearest-category similarity check during categorization (FR4).

use std::sync::Arc;

// Deliberately using LanceDB's re-exported arrow crates (`lancedb::arrow::*`)
// rather than depending on `arrow-array`/`arrow-schema` directly: LanceDB
// pins its own internal arrow version, and a separately-resolved arrow
// dependency can land on a different minor version with structurally
// identical but nominally distinct types (RecordBatch from crate A != from
// crate B), which breaks generic code like `try_collect`. See
// lancedb's own `src/arrow.rs` doc comment.
use lancedb::arrow::arrow_array::{
    Array, FixedSizeListArray, Float32Array, Int64Array, RecordBatch, RecordBatchIterator,
    StringArray, types::Float32Type,
};
use lancedb::arrow::arrow_schema::{DataType, Field, Schema, SchemaRef};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::{Connection, DistanceType};

/// Dimension of `nomic-embed-text`'s output (see plan §6). If a different
/// embedding model is substituted later, this is the one place to change.
pub const EMBEDDING_DIM: i32 = 768;

const MEMORIES_TABLE: &str = "memories";
const EXEMPLARS_TABLE: &str = "category_exemplars";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    Prompt,
    Summary,
}

impl MemoryKind {
    fn as_str(&self) -> &'static str {
        match self {
            MemoryKind::Prompt => "prompt",
            MemoryKind::Summary => "summary",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "summary" => MemoryKind::Summary,
            _ => MemoryKind::Prompt,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    pub id: String,
    pub session_id: String,
    pub kind: MemoryKind,
    pub text: String,
    pub embedding: Vec<f32>,
    pub project: String,
    /// Raw session cwd (not just the shortened `project` display name) —
    /// needed to recompute the session's real transcript path when
    /// reconstructing live board state on Core Engine restart (see
    /// `orchestrator::reconstruct_live_sessions`).
    pub cwd: String,
    pub tool: String,
    pub category: String,
    /// The session's LLM-generated title (from `categorize_session`), stored
    /// durably so `orchestrator::reconstruct_live_sessions` can restore the
    /// real title on restart instead of falling back to a crude truncation
    /// of `text` — see that function's doc comment for the bug this fixed.
    pub title: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScoredMemory {
    pub memory: Memory,
    /// Cosine distance (lower = more similar). Range [0, 2].
    pub distance: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Exemplar {
    pub category: String,
    pub exemplar_embedding: Vec<f32>,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub enum PurgeScope {
    All,
    Project(String),
}

pub struct MemoryRepo {
    db: Connection,
}

fn vector_field(name: &str) -> Field {
    Field::new(
        name,
        DataType::FixedSizeList(
            Arc::new(Field::new("item", DataType::Float32, true)),
            EMBEDDING_DIM,
        ),
        false,
    )
}

fn memories_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("session_id", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("text", DataType::Utf8, false),
        vector_field("embedding"),
        Field::new("project", DataType::Utf8, false),
        Field::new("cwd", DataType::Utf8, false),
        Field::new("tool", DataType::Utf8, false),
        Field::new("category", DataType::Utf8, false),
        Field::new("title", DataType::Utf8, false),
        Field::new("created_at", DataType::Int64, false),
    ]))
}

fn exemplars_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("category", DataType::Utf8, false),
        vector_field("exemplar_embedding"),
        Field::new("created_at", DataType::Int64, false),
    ]))
}

fn empty_batch(schema: SchemaRef) -> RecordBatch {
    RecordBatch::new_empty(schema)
}

fn memory_to_batch(m: &Memory) -> RecordBatch {
    let schema = memories_schema();
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(vec![m.id.clone()])),
            Arc::new(StringArray::from(vec![m.session_id.clone()])),
            Arc::new(StringArray::from(vec![m.kind.as_str().to_string()])),
            Arc::new(StringArray::from(vec![m.text.clone()])),
            Arc::new(
                FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
                    vec![Some(m.embedding.iter().map(|v| Some(*v)).collect::<Vec<_>>())],
                    EMBEDDING_DIM,
                ),
            ),
            Arc::new(StringArray::from(vec![m.project.clone()])),
            Arc::new(StringArray::from(vec![m.cwd.clone()])),
            Arc::new(StringArray::from(vec![m.tool.clone()])),
            Arc::new(StringArray::from(vec![m.category.clone()])),
            Arc::new(StringArray::from(vec![m.title.clone()])),
            Arc::new(Int64Array::from(vec![m.created_at])),
        ],
    )
    .expect("memory RecordBatch construction is schema-consistent by definition")
}

fn exemplar_to_batch(e: &Exemplar) -> RecordBatch {
    let schema = exemplars_schema();
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from(vec![e.category.clone()])),
            Arc::new(
                FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
                    vec![Some(
                        e.exemplar_embedding.iter().map(|v| Some(*v)).collect::<Vec<_>>(),
                    )],
                    EMBEDDING_DIM,
                ),
            ),
            Arc::new(Int64Array::from(vec![e.created_at])),
        ],
    )
    .expect("exemplar RecordBatch construction is schema-consistent by definition")
}

fn extract_string_col(batch: &RecordBatch, name: &str) -> Vec<String> {
    batch
        .column_by_name(name)
        .unwrap_or_else(|| panic!("column `{name}` missing from result batch"))
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap_or_else(|| panic!("column `{name}` is not a Utf8 array"))
        .iter()
        .map(|v| v.unwrap_or_default().to_string())
        .collect()
}

fn extract_i64_col(batch: &RecordBatch, name: &str) -> Vec<i64> {
    batch
        .column_by_name(name)
        .unwrap_or_else(|| panic!("column `{name}` missing from result batch"))
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap_or_else(|| panic!("column `{name}` is not an Int64 array"))
        .iter()
        .map(|v| v.unwrap_or_default())
        .collect()
}

fn extract_f32_col(batch: &RecordBatch, name: &str) -> Vec<f32> {
    batch
        .column_by_name(name)
        .unwrap_or_else(|| panic!("column `{name}` missing from result batch"))
        .as_any()
        .downcast_ref::<Float32Array>()
        .unwrap_or_else(|| panic!("column `{name}` is not a Float32 array"))
        .iter()
        .map(|v| v.unwrap_or_default())
        .collect()
}

fn extract_vector_col(batch: &RecordBatch, name: &str) -> Vec<Vec<f32>> {
    let col = batch
        .column_by_name(name)
        .unwrap_or_else(|| panic!("column `{name}` missing from result batch"))
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .unwrap_or_else(|| panic!("column `{name}` is not a FixedSizeList array"));
    (0..col.len())
        .map(|i| {
            col.value(i)
                .as_any()
                .downcast_ref::<Float32Array>()
                .expect("embedding items are Float32")
                .iter()
                .map(|v| v.unwrap_or_default())
                .collect()
        })
        .collect()
}

fn rows_to_memories(batch: &RecordBatch) -> Vec<Memory> {
    let ids = extract_string_col(batch, "id");
    let session_ids = extract_string_col(batch, "session_id");
    let kinds = extract_string_col(batch, "kind");
    let texts = extract_string_col(batch, "text");
    let embeddings = extract_vector_col(batch, "embedding");
    let projects = extract_string_col(batch, "project");
    let cwds = extract_string_col(batch, "cwd");
    let tools = extract_string_col(batch, "tool");
    let categories = extract_string_col(batch, "category");
    let titles = extract_string_col(batch, "title");
    let created_ats = extract_i64_col(batch, "created_at");

    (0..batch.num_rows())
        .map(|i| Memory {
            id: ids[i].clone(),
            session_id: session_ids[i].clone(),
            kind: MemoryKind::from_str(&kinds[i]),
            text: texts[i].clone(),
            embedding: embeddings[i].clone(),
            project: projects[i].clone(),
            cwd: cwds[i].clone(),
            tool: tools[i].clone(),
            category: categories[i].clone(),
            title: titles[i].clone(),
            created_at: created_ats[i],
        })
        .collect()
}

/// Recreates `table_name` (dropping any existing data) if the table already
/// exists but its on-disk schema doesn't have all the columns `expected`
/// defines — i.e. it was created by an older version of this code before a
/// field was added. There's no real migration story yet (plan §6/§10 treat
/// durable memory as resettable search/category history, not data requiring
/// perfect preservation across schema changes during active development) —
/// but silently panicking the first time a query touches a missing column,
/// as opposed to fixing it once here at startup, is not an acceptable
/// failure mode. Does nothing if the table doesn't exist yet (the normal
/// `create_table` path in `open` handles that) or already matches.
async fn recreate_if_schema_stale(
    db: &Connection,
    table_name: &str,
    existing_tables: &[String],
    expected: SchemaRef,
) -> lancedb::Result<()> {
    if !existing_tables.iter().any(|t| t == table_name) {
        return Ok(());
    }
    let table = db.open_table(table_name).execute().await?;
    let actual = table.schema().await?;
    let has_all_columns = expected.fields().iter().all(|f| actual.field_with_name(f.name()).is_ok());
    if !has_all_columns {
        db.drop_table(table_name, &[]).await?;
    }
    Ok(())
}

impl MemoryRepo {
    /// Connects to (and, on first run, initializes) the LanceDB database at
    /// `dir`. Idempotent: safe to call every app start. Also self-heals a
    /// stale on-disk schema from an older build (see
    /// `recreate_if_schema_stale`) rather than panicking the first time a
    /// query touches a column that doesn't exist yet in an existing table.
    pub async fn open(dir: &str) -> lancedb::Result<Self> {
        let db = lancedb::connect(dir).execute().await?;
        let existing = db.table_names().execute().await?;

        recreate_if_schema_stale(&db, MEMORIES_TABLE, &existing, memories_schema()).await?;
        recreate_if_schema_stale(&db, EXEMPLARS_TABLE, &existing, exemplars_schema()).await?;

        let existing = db.table_names().execute().await?;
        if !existing.iter().any(|t| t == MEMORIES_TABLE) {
            db.create_table(MEMORIES_TABLE, empty_batch(memories_schema()))
                .execute()
                .await?;
        }
        if !existing.iter().any(|t| t == EXEMPLARS_TABLE) {
            db.create_table(EXEMPLARS_TABLE, empty_batch(exemplars_schema()))
                .execute()
                .await?;
        }

        Ok(Self { db })
    }

    /// Insert-or-replace a memory row by `id` (see plan §2: "upsert" here means
    /// merge-on-key, not append-only — a session's task-summary row can be
    /// refreshed in place rather than accumulating duplicates).
    pub async fn upsert_memory(&self, m: &Memory) -> lancedb::Result<()> {
        let table = self.db.open_table(MEMORIES_TABLE).execute().await?;
        let batch = memory_to_batch(m);
        let schema = batch.schema();
        let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
        let mut builder = table.merge_insert(&["id"]);
        builder.when_matched_update_all(None);
        builder.when_not_matched_insert_all();
        builder.execute(Box::new(reader)).await?;
        Ok(())
    }

    /// Insert-or-replace a category's exemplar embedding, keyed on `category`.
    pub async fn upsert_exemplar(&self, e: &Exemplar) -> lancedb::Result<()> {
        let table = self.db.open_table(EXEMPLARS_TABLE).execute().await?;
        let batch = exemplar_to_batch(e);
        let schema = batch.schema();
        let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
        let mut builder = table.merge_insert(&["category"]);
        builder.when_matched_update_all(None);
        builder.when_not_matched_insert_all();
        builder.execute(Box::new(reader)).await?;
        Ok(())
    }

    /// Creates a new, empty category (Phase 2 §5 — user-created categories
    /// via the board's "+ New category" control). There's no session prompt
    /// to seed the exemplar from yet, so the category name's own embedding
    /// is used as the placeholder — the same `upsert_exemplar` mechanism
    /// every other category's exemplar goes through, just seeded
    /// differently. A later real session dropped into this lane recategorizes
    /// exactly like any other manual override; the exemplar isn't refined
    /// further here.
    pub async fn create_category(&self, name: &str, embedding: Vec<f32>, created_at: i64) -> lancedb::Result<()> {
        self.upsert_exemplar(&Exemplar { category: name.to_string(), exemplar_embedding: embedding, created_at }).await
    }

    /// Nearest category exemplar to `embedding` by cosine distance, if any
    /// exemplars exist yet. The caller (categorization pipeline, plan §5)
    /// applies the configurable similarity threshold to this result.
    pub async fn nearest_category(
        &self,
        embedding: &[f32],
    ) -> lancedb::Result<Option<(String, f32)>> {
        let table = self.db.open_table(EXEMPLARS_TABLE).execute().await?;
        let batches: Vec<RecordBatch> = table
            .query()
            .nearest_to(embedding)?
            .column("exemplar_embedding")
            .distance_type(DistanceType::Cosine)
            .limit(1)
            .execute()
            .await?
            .try_collect()
            .await?;

        for batch in &batches {
            if batch.num_rows() == 0 {
                continue;
            }
            let categories = extract_string_col(batch, "category");
            let distances = extract_f32_col(batch, "_distance");
            return Ok(Some((categories[0].clone(), distances[0])));
        }
        Ok(None)
    }

    /// All known category exemplars (small table — one row per category).
    pub async fn list_exemplars(&self) -> lancedb::Result<Vec<Exemplar>> {
        let table = self.db.open_table(EXEMPLARS_TABLE).execute().await?;
        let batches: Vec<RecordBatch> = table.query().execute().await?.try_collect().await?;
        let mut out = Vec::new();
        for batch in &batches {
            let categories = extract_string_col(batch, "category");
            let embeddings = extract_vector_col(batch, "exemplar_embedding");
            let created_ats = extract_i64_col(batch, "created_at");
            for i in 0..batch.num_rows() {
                out.push(Exemplar {
                    category: categories[i].clone(),
                    exemplar_embedding: embeddings[i].clone(),
                    created_at: created_ats[i],
                });
            }
        }
        Ok(out)
    }

    /// Semantic search over `memories` (live + historical) ranked by cosine
    /// distance — backs FR6 / the ⌘K overlay's server round trip.
    pub async fn search(&self, embedding: &[f32], limit: usize) -> lancedb::Result<Vec<ScoredMemory>> {
        let table = self.db.open_table(MEMORIES_TABLE).execute().await?;
        let batches: Vec<RecordBatch> = table
            .query()
            .nearest_to(embedding)?
            .column("embedding")
            .distance_type(DistanceType::Cosine)
            .limit(limit)
            .execute()
            .await?
            .try_collect()
            .await?;

        let mut out = Vec::new();
        for batch in &batches {
            if batch.num_rows() == 0 {
                continue;
            }
            let distances = extract_f32_col(batch, "_distance");
            for (i, memory) in rows_to_memories(batch).into_iter().enumerate() {
                out.push(ScoredMemory {
                    memory,
                    distance: distances[i],
                });
            }
        }
        Ok(out)
    }

    /// Every durable memory row (one per session, in practice — see
    /// `categorize_session`, which always upserts a single `{session_id}-prompt`
    /// row per session). Used on Core Engine restart to reconstruct which
    /// sessions were previously seen and what category they resolved to
    /// (`orchestrator::reconstruct_live_sessions`), and could back a future
    /// "browse full history" view.
    pub async fn list_session_memories(&self) -> lancedb::Result<Vec<Memory>> {
        let table = self.db.open_table(MEMORIES_TABLE).execute().await?;
        let batches: Vec<RecordBatch> = table.query().execute().await?.try_collect().await?;
        Ok(batches.iter().flat_map(rows_to_memories).collect())
    }

    /// Explicit, user-triggered purge of durable memory (plan §6 retention
    /// policy — no automatic TTL/expiry job exists; this is the only way rows
    /// leave `memories`).
    pub async fn purge(&self, scope: PurgeScope) -> lancedb::Result<()> {
        let table = self.db.open_table(MEMORIES_TABLE).execute().await?;
        let predicate = match scope {
            PurgeScope::All => "true".to_string(),
            PurgeScope::Project(project) => format!("project = '{}'", project.replace('\'', "''")),
        };
        table.delete(&predicate).await?;
        Ok(())
    }

    /// Removes a category's exemplar row (the board's "Delete category"
    /// action) so it stops appearing as a known lane. Deliberately doesn't
    /// touch `memories` — the historical prompt/summary rows that were once
    /// tagged with this category stay searchable via Ask Memory; only the
    /// live cascade (removing each currently-live session in the category)
    /// is the caller's job (`api::delete_category`), same separation as
    /// single-session delete leaving `memories` alone.
    pub async fn delete_exemplar(&self, category: &str) -> lancedb::Result<()> {
        let table = self.db.open_table(EXEMPLARS_TABLE).execute().await?;
        let predicate = format!("category = '{}'", category.replace('\'', "''"));
        table.delete(&predicate).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vec_of(dim: i32, fill: f32) -> Vec<f32> {
        vec![fill; dim as usize]
    }

    /// Regression test for a real bug: an on-disk `memories` table created by
    /// an older build (missing the later-added `cwd` column) used to make
    /// every subsequent query panic ("column `cwd` missing from result
    /// batch") instead of `open()` noticing and recreating the table fresh.
    #[tokio::test]
    async fn open_recreates_a_table_with_a_stale_pre_cwd_schema() {
        let dir = tempfile::tempdir().unwrap();

        // Simulate a table created before the `cwd` column existed, by
        // building it directly against the old (smaller) schema rather than
        // going through `MemoryRepo::open`.
        let old_schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new("session_id", DataType::Utf8, false),
            Field::new("kind", DataType::Utf8, false),
            Field::new("text", DataType::Utf8, false),
            vector_field("embedding"),
            Field::new("project", DataType::Utf8, false),
            Field::new("tool", DataType::Utf8, false),
            Field::new("category", DataType::Utf8, false),
            Field::new("created_at", DataType::Int64, false),
        ]));
        let db = lancedb::connect(dir.path().to_str().unwrap()).execute().await.unwrap();
        db.create_table(MEMORIES_TABLE, empty_batch(old_schema)).execute().await.unwrap();

        // Opening through the real repo must notice the mismatch, recreate
        // the table, and behave completely normally afterward — not panic.
        let repo = MemoryRepo::open(dir.path().to_str().unwrap()).await.unwrap();
        let m = Memory {
            id: "m1".into(),
            session_id: "s1".into(),
            kind: MemoryKind::Prompt,
            text: "refactor auth middleware".into(),
            embedding: vec_of(EMBEDDING_DIM, 1.0),
            project: "api-gateway".into(),
            cwd: "/Users/omricohen/api-gateway".into(),
            tool: "Claude Code".into(),
            category: "Backend / API".into(),
            title: "Refactor auth middleware".into(),
            created_at: 0,
        };
        repo.upsert_memory(&m).await.unwrap();

        let results = repo.search(&vec_of(EMBEDDING_DIM, 1.0), 5).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory.cwd, "/Users/omricohen/api-gateway");
    }

    #[tokio::test]
    async fn upsert_and_search_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::open(dir.path().to_str().unwrap()).await.unwrap();

        let m = Memory {
            id: "m1".into(),
            session_id: "s1".into(),
            kind: MemoryKind::Prompt,
            text: "refactor auth middleware to async".into(),
            embedding: vec_of(EMBEDDING_DIM, 1.0),
            project: "api-gateway".into(),
            cwd: "/Users/omricohen/api-gateway".into(),
            tool: "Claude Code".into(),
            category: "Backend / API".into(),
            title: "Refactor auth middleware".into(),
            created_at: 1_700_000_000_000,
        };
        repo.upsert_memory(&m).await.unwrap();

        let results = repo.search(&vec_of(EMBEDDING_DIM, 1.0), 5).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory.id, "m1");
        assert!(results[0].distance < 0.01, "identical vector should have ~0 cosine distance");

        // Upsert with the same id should replace, not duplicate.
        let mut updated = m.clone();
        updated.text = "refactor auth middleware — done".into();
        repo.upsert_memory(&updated).await.unwrap();
        let results = repo.search(&vec_of(EMBEDDING_DIM, 1.0), 5).await.unwrap();
        assert_eq!(results.len(), 1, "upsert by id must replace, not duplicate");
        assert_eq!(results[0].memory.text, "refactor auth middleware — done");
    }

    #[tokio::test]
    async fn nearest_category_threshold_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::open(dir.path().to_str().unwrap()).await.unwrap();

        // No exemplars yet -> no match.
        assert_eq!(repo.nearest_category(&vec_of(EMBEDDING_DIM, 1.0)).await.unwrap(), None);

        repo.upsert_exemplar(&Exemplar {
            category: "Backend / API".into(),
            exemplar_embedding: vec_of(EMBEDDING_DIM, 1.0),
            created_at: 0,
        })
        .await
        .unwrap();
        repo.upsert_exemplar(&Exemplar {
            category: "Frontend".into(),
            exemplar_embedding: vec_of(EMBEDDING_DIM, -1.0),
            created_at: 0,
        })
        .await
        .unwrap();

        // Identical vector to the "Backend / API" exemplar -> distance ~0 (clearly above any sane threshold).
        let (cat, dist) = repo
            .nearest_category(&vec_of(EMBEDDING_DIM, 1.0))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cat, "Backend / API");
        assert!(dist < 0.01);

        // Opposite-direction vector -> should match "Frontend" (nearest), with
        // a large cosine distance (near the max of 2.0) — this is the "clearly
        // below threshold, should NOT join" case the categorization pipeline
        // checks (plan §5 step 4).
        let (cat, dist) = repo
            .nearest_category(&vec_of(EMBEDDING_DIM, -1.0))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cat, "Frontend");
        assert!(dist < 0.01);
    }

    #[tokio::test]
    async fn create_category_seeds_an_exemplar_findable_via_list_and_nearest() {
        let dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::open(dir.path().to_str().unwrap()).await.unwrap();

        repo.create_category("Ops / Infra", vec_of(EMBEDDING_DIM, 1.0), 42).await.unwrap();

        let exemplars = repo.list_exemplars().await.unwrap();
        assert_eq!(exemplars.len(), 1);
        assert_eq!(exemplars[0].category, "Ops / Infra");
        assert_eq!(exemplars[0].created_at, 42);

        // A brand-new empty category must be reachable the same way any
        // other category's exemplar is — a manual recategorize into it is
        // just `POST /sessions/:id/recategorize`, but the lane itself has to
        // exist first via this seeded exemplar.
        let (cat, _) = repo.nearest_category(&vec_of(EMBEDDING_DIM, 1.0)).await.unwrap().unwrap();
        assert_eq!(cat, "Ops / Infra");
    }

    #[tokio::test]
    async fn delete_exemplar_removes_only_the_named_category_and_leaves_memories_alone() {
        let dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::open(dir.path().to_str().unwrap()).await.unwrap();

        repo.create_category("Ops / Infra", vec_of(EMBEDDING_DIM, 1.0), 42).await.unwrap();
        repo.create_category("Backend / API", vec_of(EMBEDDING_DIM, -1.0), 43).await.unwrap();
        repo.upsert_memory(&Memory {
            id: "a".into(),
            session_id: "a".into(),
            kind: MemoryKind::Prompt,
            text: "x".into(),
            embedding: vec_of(EMBEDDING_DIM, 1.0),
            project: "proj-1".into(),
            cwd: "/Users/omricohen/proj-1".into(),
            tool: "Claude Code".into(),
            category: "Ops / Infra".into(),
            title: "x".into(),
            created_at: 0,
        })
        .await
        .unwrap();

        repo.delete_exemplar("Ops / Infra").await.unwrap();

        let exemplars = repo.list_exemplars().await.unwrap();
        assert_eq!(exemplars.len(), 1, "only the named category's exemplar should be removed");
        assert_eq!(exemplars[0].category, "Backend / API");

        let memories = repo.list_session_memories().await.unwrap();
        assert_eq!(memories.len(), 1, "historical memory rows must survive a category delete");
        assert_eq!(memories[0].category, "Ops / Infra");
    }

    #[tokio::test]
    async fn purge_by_project_and_all() {
        let dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::open(dir.path().to_str().unwrap()).await.unwrap();

        for (id, project) in [("a", "proj-1"), ("b", "proj-1"), ("c", "proj-2")] {
            repo.upsert_memory(&Memory {
                id: id.into(),
                session_id: id.into(),
                kind: MemoryKind::Prompt,
                text: "x".into(),
                embedding: vec_of(EMBEDDING_DIM, 1.0),
                project: project.into(),
                cwd: format!("/Users/omricohen/{project}"),
                tool: "Claude Code".into(),
                category: "Backend / API".into(),
                title: "x".into(),
                created_at: 0,
            })
            .await
            .unwrap();
        }

        repo.purge(PurgeScope::Project("proj-1".into())).await.unwrap();
        let remaining = repo.search(&vec_of(EMBEDDING_DIM, 1.0), 10).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].memory.project, "proj-2");

        repo.purge(PurgeScope::All).await.unwrap();
        let remaining = repo.search(&vec_of(EMBEDDING_DIM, 1.0), 10).await.unwrap();
        assert_eq!(remaining.len(), 0);
    }
}
