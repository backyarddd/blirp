//! Project memory: records, briefs (+ history), wiki pages, suggestions, resources.

use super::{Change, Result, Store, StoreError, all, apply_in, json_col, one};
use crate::model::{
    Brief, BriefProposal, Record, RecordKind, RecordProposal, RecordStatus, Resource, Suggestion,
    SuggestionStatus, SuggestionTarget, WikiPage, WikiProposal,
};
use rusqlite::{Connection, Row, Transaction, params};

pub(super) fn record_row(r: &Row<'_>) -> rusqlite::Result<Record> {
    Ok(Record {
        id: r.get("id")?,
        project_id: r.get("project_id")?,
        kind: r.get("kind")?,
        title: r.get("title")?,
        body: r.get("body")?,
        status: r.get("status")?,
        pinned: r.get("pinned")?,
        source_session_id: r.get("source_session_id")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        updated_by: r.get("updated_by")?,
    })
}

fn brief_row(r: &Row<'_>) -> rusqlite::Result<Brief> {
    Ok(Brief {
        id: r.get("id")?,
        project_id: r.get("project_id")?,
        body_md: r.get("body_md")?,
        version: r.get("version")?,
        updated_at: r.get("updated_at")?,
        updated_by: r.get("updated_by")?,
        machine_id: r.get("machine_id")?,
    })
}

/// Append `b` to the brief history (a row with its id already there is
/// left alone). Changes from older versions carry no id; they get the
/// deterministic legacy id migration 6 gave existing rows. Returns the id.
pub(super) fn insert_brief_history(tx: &Transaction<'_>, b: &Brief) -> Result<String> {
    let id = if b.id.is_empty() {
        format!("legacy-{}-{}", b.project_id, b.version)
    } else {
        b.id.clone()
    };
    tx.execute(
        "INSERT OR IGNORE INTO brief_history(id, project_id, body_md, updated_at, updated_by, machine_id)
         VALUES (?1,?2,?3,?4,?5,?6)",
        params![id, b.project_id, b.body_md, b.updated_at, b.updated_by, b.machine_id],
    )?;
    Ok(id)
}

/// History rows of a project with their derived version (1 = oldest).
const HISTORY: &str = "SELECT id, project_id, body_md, updated_at, updated_by, machine_id,
       row_number() OVER (ORDER BY updated_at, id) AS version
     FROM brief_history WHERE project_id = ?1";

pub(super) fn wiki_row(r: &Row<'_>) -> rusqlite::Result<WikiPage> {
    Ok(WikiPage {
        id: r.get("id")?,
        project_id: r.get("project_id")?,
        slug: r.get("slug")?,
        title: r.get("title")?,
        body_md: r.get("body_md")?,
        updated_at: r.get("updated_at")?,
        updated_by: r.get("updated_by")?,
        deleted: r.get("deleted")?,
    })
}

pub(super) fn resource_row(r: &Row<'_>) -> rusqlite::Result<Resource> {
    Ok(Resource {
        id: r.get("id")?,
        project_id: r.get("project_id")?,
        kind: r.get("kind")?,
        url: r.get("url")?,
        title: r.get("title")?,
        meta: json_col(r, "meta_json")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
        deleted: r.get("deleted")?,
    })
}

fn suggestion_row(r: &Row<'_>) -> rusqlite::Result<Suggestion> {
    Ok(Suggestion {
        id: r.get("id")?,
        project_id: r.get("project_id")?,
        target: r.get("target")?,
        target_id: r.get("target_id")?,
        proposal: json_col(r, "proposal_json")?.unwrap_or_default(),
        rationale: r.get("rationale")?,
        source_session_id: r.get("source_session_id")?,
        status: r.get("status")?,
        created_at: r.get("created_at")?,
        decided_at: r.get("decided_at")?,
    })
}

/// All brief versions of a project, newest first.
pub(super) fn history_in(c: &Connection, project_id: &str) -> Result<Vec<Brief>> {
    all(
        c,
        &format!("{HISTORY} ORDER BY updated_at DESC, id DESC"),
        params![project_id],
        brief_row,
    )
}

pub(super) fn get_brief_in(c: &Connection, project_id: &str) -> Result<Option<Brief>> {
    one(
        c,
        "SELECT b.history_id AS id, b.project_id, b.body_md, b.updated_at, b.updated_by, b.machine_id,
           (SELECT count(*) FROM brief_history h WHERE h.project_id = b.project_id
              AND (h.updated_at, h.id) <= (b.updated_at, b.history_id)) AS version
         FROM briefs b WHERE b.project_id = ?1",
        params![project_id],
        brief_row,
    )
}

pub(super) fn put_brief_in(
    tx: &Transaction<'_>,
    project_id: &str,
    body_md: &str,
    by: &str,
) -> Result<Brief> {
    let count: i64 = tx.query_row(
        "SELECT count(*) FROM brief_history WHERE project_id = ?1",
        params![project_id],
        |r| r.get(0),
    )?;
    // After the newest version known here (another machine's clock may be
    // ahead), so it is the newest in the derived order too.
    let newest: Option<i64> = tx.query_row(
        "SELECT max(updated_at) FROM brief_history WHERE project_id = ?1",
        params![project_id],
        |r| r.get(0),
    )?;
    let b = Brief {
        id: crate::new_id(),
        project_id: project_id.to_string(),
        body_md: body_md.to_string(),
        version: count + 1,
        updated_at: crate::now_ms().max(newest.map_or(0, |n| n + 1)),
        updated_by: by.to_string(),
        machine_id: super::local_machine_in(tx)?,
    };
    apply_in(tx, &Change::Brief(b.clone()))?;
    Ok(b)
}

fn get_wiki_in(c: &Connection, project_id: &str, slug: &str) -> Result<Option<WikiPage>> {
    one(
        c,
        "SELECT * FROM wiki_pages WHERE project_id = ?1 AND slug = ?2",
        params![project_id, slug],
        wiki_row,
    )
}

/// Wiki slugs: 1-100 chars of `[a-z0-9-]`, not starting or ending with `-`.
pub fn validate_slug(slug: &str) -> Result<()> {
    let ok = (1..=100).contains(&slug.len())
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !slug.starts_with('-')
        && !slug.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err(StoreError::Invalid(format!(
            "slug {slug:?} must be 1-100 chars of [a-z0-9-] without leading/trailing '-'"
        )))
    }
}

fn nonempty(field: &str, v: &str) -> Result<()> {
    if v.trim().is_empty() {
        Err(StoreError::Invalid(format!("{field} must not be empty")))
    } else {
        Ok(())
    }
}

fn create_wiki_in(
    tx: &Transaction<'_>,
    project_id: &str,
    slug: &str,
    title: &str,
    body_md: &str,
    by: &str,
) -> Result<WikiPage> {
    validate_slug(slug)?;
    nonempty("title", title)?;
    let existing = get_wiki_in(tx, project_id, slug)?;
    if existing.as_ref().is_some_and(|w| !w.deleted) {
        return Err(StoreError::Conflict(format!(
            "wiki page {slug:?} already exists"
        )));
    }
    let page = WikiPage {
        // Re-creating a deleted slug revives its row (slug is unique per project).
        id: existing.map_or_else(crate::new_id, |w| w.id),
        project_id: project_id.to_string(),
        slug: slug.to_string(),
        title: title.to_string(),
        body_md: body_md.to_string(),
        updated_at: crate::now_ms(),
        updated_by: by.to_string(),
        deleted: false,
    };
    apply_in(tx, &Change::WikiPage(page.clone()))?;
    Ok(page)
}

#[derive(Debug, Clone, Default)]
pub struct RecordFilter {
    pub status: Option<RecordStatus>,
    pub kind: Option<RecordKind>,
}

impl Store {
    // ---------------------------------------------------------------- briefs

    pub fn get_brief(&self, project_id: &str) -> Result<Option<Brief>> {
        self.read(|c| get_brief_in(c, project_id))
    }

    /// Write a new brief version (history is kept).
    pub fn put_brief(&self, project_id: &str, body_md: &str, by: &str) -> Result<Brief> {
        self.write(|tx| put_brief_in(tx, project_id, body_md, by))
    }

    /// All brief versions, newest first.
    pub fn brief_history(&self, project_id: &str) -> Result<Vec<Brief>> {
        self.read(|c| history_in(c, project_id))
    }

    /// Write the body of history entry `version` as a new version.
    pub fn revert_brief(&self, project_id: &str, version: i64, by: &str) -> Result<Brief> {
        self.write(|tx| {
            let old: String = one(
                tx,
                &format!("SELECT body_md FROM ({HISTORY}) WHERE version = ?2"),
                params![project_id, version],
                |r| r.get(0),
            )?
            .ok_or(StoreError::NotFound("brief version"))?;
            put_brief_in(tx, project_id, &old, by)
        })
    }

    /// Write the body of history entry `id` as a new version.
    pub fn revert_brief_to(&self, project_id: &str, id: &str, by: &str) -> Result<Brief> {
        self.write(|tx| {
            let old: String = one(
                tx,
                "SELECT body_md FROM brief_history WHERE project_id = ?1 AND id = ?2",
                params![project_id, id],
                |r| r.get(0),
            )?
            .ok_or(StoreError::NotFound("brief version"))?;
            put_brief_in(tx, project_id, &old, by)
        })
    }

    // ---------------------------------------------------------------- records

    /// Records of a project, pinned first then most recently updated.
    pub fn list_records(&self, project_id: &str, f: &RecordFilter) -> Result<Vec<Record>> {
        self.read(|c| {
            all(
                c,
                "SELECT * FROM records WHERE project_id = ?1
                   AND (?2 IS NULL OR status = ?2) AND (?3 IS NULL OR kind = ?3)
                 ORDER BY pinned DESC, updated_at DESC",
                params![project_id, f.status, f.kind],
                record_row,
            )
        })
    }

    pub fn get_record(&self, id: &str) -> Result<Option<Record>> {
        self.read(|c| {
            one(
                c,
                "SELECT * FROM records WHERE id = ?1",
                params![id],
                record_row,
            )
        })
    }

    pub fn create_record(&self, record: Record) -> Result<Record> {
        nonempty("title", &record.title)?;
        self.apply(Change::Record(record.clone()))?;
        Ok(record)
    }

    /// Read-modify-write a record in one transaction.
    pub fn modify_record(&self, id: &str, f: impl FnOnce(&mut Record)) -> Result<Record> {
        self.write(|tx| {
            let mut r = one(
                tx,
                "SELECT * FROM records WHERE id = ?1",
                params![id],
                record_row,
            )?
            .ok_or(StoreError::NotFound("record"))?;
            f(&mut r);
            nonempty("title", &r.title)?;
            r.updated_at = crate::now_ms();
            apply_in(tx, &Change::Record(r.clone()))?;
            Ok(r)
        })
    }

    pub fn delete_record(&self, id: &str) -> Result<()> {
        self.write(|tx| {
            if one(
                tx,
                "SELECT 1 FROM records WHERE id = ?1",
                params![id],
                |_| Ok(()),
            )?
            .is_none()
            {
                return Err(StoreError::NotFound("record"));
            }
            apply_in(tx, &Change::DeleteRecord { id: id.to_string() })?;
            Ok(())
        })
    }

    // ---------------------------------------------------------------- wiki

    pub fn list_wiki(&self, project_id: &str) -> Result<Vec<WikiPage>> {
        self.read(|c| {
            all(
                c,
                "SELECT * FROM wiki_pages WHERE project_id = ?1 AND deleted = 0 ORDER BY title COLLATE NOCASE",
                params![project_id],
                wiki_row,
            )
        })
    }

    /// A live (not deleted) wiki page.
    pub fn get_wiki_page(&self, project_id: &str, slug: &str) -> Result<Option<WikiPage>> {
        Ok(self
            .read(|c| get_wiki_in(c, project_id, slug))?
            .filter(|w| !w.deleted))
    }

    pub fn create_wiki_page(
        &self,
        project_id: &str,
        slug: &str,
        title: &str,
        body_md: &str,
        by: &str,
    ) -> Result<WikiPage> {
        self.write(|tx| create_wiki_in(tx, project_id, slug, title, body_md, by))
    }

    pub fn update_wiki_page(
        &self,
        project_id: &str,
        slug: &str,
        title: &str,
        body_md: &str,
        by: &str,
    ) -> Result<WikiPage> {
        nonempty("title", title)?;
        self.write(|tx| {
            let mut w = get_wiki_in(tx, project_id, slug)?
                .filter(|w| !w.deleted)
                .ok_or(StoreError::NotFound("wiki page"))?;
            w.title = title.to_string();
            w.body_md = body_md.to_string();
            w.updated_at = crate::now_ms();
            w.updated_by = by.to_string();
            apply_in(tx, &Change::WikiPage(w.clone()))?;
            Ok(w)
        })
    }

    pub fn delete_wiki_page(&self, project_id: &str, slug: &str, by: &str) -> Result<()> {
        self.write(|tx| {
            let mut w = get_wiki_in(tx, project_id, slug)?
                .filter(|w| !w.deleted)
                .ok_or(StoreError::NotFound("wiki page"))?;
            w.deleted = true;
            w.updated_at = crate::now_ms();
            w.updated_by = by.to_string();
            apply_in(tx, &Change::WikiPage(w))?;
            Ok(())
        })
    }

    // ---------------------------------------------------------------- resources

    pub fn list_resources(&self, project_id: &str) -> Result<Vec<Resource>> {
        self.read(|c| {
            all(
                c,
                "SELECT * FROM resources WHERE project_id = ?1 AND deleted = 0 ORDER BY created_at DESC",
                params![project_id],
                resource_row,
            )
        })
    }

    /// A live (not deleted) resource.
    pub fn get_resource(&self, id: &str) -> Result<Option<Resource>> {
        Ok(self
            .read(|c| {
                one(
                    c,
                    "SELECT * FROM resources WHERE id = ?1",
                    params![id],
                    resource_row,
                )
            })?
            .filter(|r| !r.deleted))
    }

    pub fn create_resource(&self, r: Resource) -> Result<Resource> {
        nonempty("url", &r.url)?;
        self.apply(Change::Resource(r.clone()))?;
        Ok(r)
    }

    /// Read-modify-write a live resource in one transaction.
    pub fn modify_resource(&self, id: &str, f: impl FnOnce(&mut Resource)) -> Result<Resource> {
        self.write(|tx| {
            let mut r = one(
                tx,
                "SELECT * FROM resources WHERE id = ?1",
                params![id],
                resource_row,
            )?
            .filter(|r| !r.deleted)
            .ok_or(StoreError::NotFound("resource"))?;
            f(&mut r);
            nonempty("url", &r.url)?;
            r.updated_at = crate::now_ms();
            apply_in(tx, &Change::Resource(r.clone()))?;
            Ok(r)
        })
    }

    // ---------------------------------------------------------------- suggestions

    pub fn list_suggestions(
        &self,
        project_id: &str,
        status: Option<SuggestionStatus>,
    ) -> Result<Vec<Suggestion>> {
        self.read(|c| {
            all(
                c,
                "SELECT * FROM suggestions WHERE project_id = ?1 AND (?2 IS NULL OR status = ?2)
                 ORDER BY created_at DESC",
                params![project_id, status],
                suggestion_row,
            )
        })
    }

    pub fn get_suggestion(&self, id: &str) -> Result<Option<Suggestion>> {
        self.read(|c| {
            one(
                c,
                "SELECT * FROM suggestions WHERE id = ?1",
                params![id],
                suggestion_row,
            )
        })
    }

    /// Suggestions are local (not replicated), so this writes directly.
    pub fn insert_suggestion(&self, s: &Suggestion) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO suggestions(id, project_id, target, target_id, proposal_json, rationale,
                   source_session_id, status, created_at, decided_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    s.id, s.project_id, s.target, s.target_id, s.proposal.to_string(), s.rationale,
                    s.source_session_id, s.status, s.created_at, s.decided_at
                ],
            )?;
            Ok(())
        })
    }

    /// Decide a pending suggestion. Accepting applies its proposal to the
    /// target in the same transaction.
    pub fn decide_suggestion(
        &self,
        id: &str,
        decision: SuggestionStatus,
        by: &str,
    ) -> Result<Suggestion> {
        if decision == SuggestionStatus::Pending {
            return Err(StoreError::Invalid("decision must not be pending".into()));
        }
        self.write(|tx| {
            let mut s = one(
                tx,
                "SELECT * FROM suggestions WHERE id = ?1",
                params![id],
                suggestion_row,
            )?
            .ok_or(StoreError::NotFound("suggestion"))?;
            if s.status != SuggestionStatus::Pending {
                return Err(StoreError::Conflict(format!(
                    "suggestion is already {}",
                    s.status
                )));
            }
            if decision == SuggestionStatus::Accepted {
                apply_proposal(tx, &s, by)?;
            }
            s.status = decision;
            s.decided_at = Some(crate::now_ms());
            tx.execute(
                "UPDATE suggestions SET status = ?1, decided_at = ?2 WHERE id = ?3",
                params![s.status, s.decided_at, s.id],
            )?;
            Ok(s)
        })
    }
}

fn parse_proposal<T: serde::de::DeserializeOwned>(s: &Suggestion) -> Result<T> {
    serde_json::from_value(s.proposal.clone()).map_err(|e| {
        StoreError::Invalid(format!(
            "suggestion proposal is not a valid {}: {e}",
            s.target
        ))
    })
}

fn apply_proposal(tx: &Transaction<'_>, s: &Suggestion, by: &str) -> Result<()> {
    let now = crate::now_ms();
    match s.target {
        SuggestionTarget::Brief => {
            let p: BriefProposal = parse_proposal(s)?;
            put_brief_in(tx, &s.project_id, &p.body_md, by)?;
        }
        SuggestionTarget::Record => {
            let p: RecordProposal = parse_proposal(s)?;
            nonempty("title", &p.title)?;
            let existing = match &s.target_id {
                Some(id) => Some(
                    one(
                        tx,
                        "SELECT * FROM records WHERE id = ?1",
                        params![id],
                        record_row,
                    )?
                    .ok_or(StoreError::NotFound("record"))?,
                ),
                None => None,
            };
            let record = match existing {
                Some(mut r) => {
                    r.kind = p.kind;
                    r.title = p.title;
                    r.body = p.body;
                    if let Some(st) = p.status {
                        r.status = st;
                    }
                    r.updated_at = now;
                    r.updated_by = by.to_string();
                    r
                }
                None => Record {
                    id: crate::new_id(),
                    project_id: s.project_id.clone(),
                    kind: p.kind,
                    title: p.title,
                    body: p.body,
                    status: p.status.unwrap_or(RecordStatus::Active),
                    pinned: false,
                    source_session_id: s.source_session_id.clone(),
                    created_at: now,
                    updated_at: now,
                    updated_by: by.to_string(),
                },
            };
            apply_in(tx, &Change::Record(record))?;
        }
        SuggestionTarget::Wiki => {
            let p: WikiProposal = parse_proposal(s)?;
            match get_wiki_in(tx, &s.project_id, &p.slug)?.filter(|w| !w.deleted) {
                Some(mut w) => {
                    nonempty("title", &p.title)?;
                    w.title = p.title;
                    w.body_md = p.body_md;
                    w.updated_at = now;
                    w.updated_by = by.to_string();
                    apply_in(tx, &Change::WikiPage(w))?;
                }
                None => {
                    create_wiki_in(tx, &s.project_id, &p.slug, &p.title, &p.body_md, by)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::temp_store;
    use super::*;
    use crate::model::RecordKind;
    use serde_json::json;

    fn suggestion(
        target: SuggestionTarget,
        target_id: Option<&str>,
        proposal: serde_json::Value,
    ) -> Suggestion {
        Suggestion {
            id: crate::new_id(),
            project_id: "p".into(),
            target,
            target_id: target_id.map(str::to_string),
            proposal,
            rationale: "because".into(),
            source_session_id: None,
            status: SuggestionStatus::Pending,
            created_at: crate::now_ms(),
            decided_at: None,
        }
    }

    #[test]
    fn accepting_suggestions_applies_them() {
        let (_d, store) = temp_store();
        let b = suggestion(
            SuggestionTarget::Brief,
            None,
            json!({"body_md": "new brief"}),
        );
        store.insert_suggestion(&b).unwrap();
        let decided = store
            .decide_suggestion(&b.id, SuggestionStatus::Accepted, "user")
            .unwrap();
        assert_eq!(decided.status, SuggestionStatus::Accepted);
        assert_eq!(store.get_brief("p").unwrap().unwrap().body_md, "new brief");
        assert!(matches!(
            store.decide_suggestion(&b.id, SuggestionStatus::Rejected, "user"),
            Err(StoreError::Conflict(_))
        ));

        let r = suggestion(
            SuggestionTarget::Record,
            None,
            json!({"kind": "gotcha", "title": "t", "body": "b"}),
        );
        store.insert_suggestion(&r).unwrap();
        store
            .decide_suggestion(&r.id, SuggestionStatus::Accepted, "user")
            .unwrap();
        let recs = store.list_records("p", &RecordFilter::default()).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].kind, RecordKind::Gotcha);

        let upd = suggestion(
            SuggestionTarget::Record,
            Some(&recs[0].id),
            json!({"kind": "gotcha", "title": "t2", "body": "b2", "status": "resolved"}),
        );
        store.insert_suggestion(&upd).unwrap();
        store
            .decide_suggestion(&upd.id, SuggestionStatus::Accepted, "user")
            .unwrap();
        let rec = store.get_record(&recs[0].id).unwrap().unwrap();
        assert_eq!(
            (rec.title.as_str(), rec.status),
            ("t2", RecordStatus::Resolved)
        );

        let w = suggestion(
            SuggestionTarget::Wiki,
            None,
            json!({"slug": "setup", "title": "Setup", "body_md": "x"}),
        );
        store.insert_suggestion(&w).unwrap();
        store
            .decide_suggestion(&w.id, SuggestionStatus::Accepted, "user")
            .unwrap();
        assert_eq!(
            store.get_wiki_page("p", "setup").unwrap().unwrap().title,
            "Setup"
        );

        let bad = suggestion(SuggestionTarget::Wiki, None, json!({"nope": 1}));
        store.insert_suggestion(&bad).unwrap();
        assert!(matches!(
            store.decide_suggestion(&bad.id, SuggestionStatus::Accepted, "user"),
            Err(StoreError::Invalid(_))
        ));
        // Failed accept left it pending; dismissing still works.
        store
            .decide_suggestion(&bad.id, SuggestionStatus::Dismissed, "user")
            .unwrap();
        assert_eq!(
            store
                .list_suggestions("p", Some(SuggestionStatus::Pending))
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn wiki_lifecycle_and_slug_rules() {
        let (_d, store) = temp_store();
        assert!(matches!(
            store.create_wiki_page("p", "Bad Slug", "t", "", "user"),
            Err(StoreError::Invalid(_))
        ));
        let w = store
            .create_wiki_page("p", "a-b", "T", "body", "user")
            .unwrap();
        assert!(matches!(
            store.create_wiki_page("p", "a-b", "T", "", "user"),
            Err(StoreError::Conflict(_))
        ));
        store
            .update_wiki_page("p", "a-b", "T2", "b2", "user")
            .unwrap();
        store.delete_wiki_page("p", "a-b", "user").unwrap();
        assert!(store.get_wiki_page("p", "a-b").unwrap().is_none());
        let again = store
            .create_wiki_page("p", "a-b", "T3", "", "user")
            .unwrap();
        assert_eq!(again.id, w.id);
    }

    #[test]
    fn records_and_resources_crud() {
        let (_d, store) = temp_store();
        let now = crate::now_ms();
        let r = store
            .create_record(Record {
                id: crate::new_id(),
                project_id: "p".into(),
                kind: RecordKind::Decision,
                title: "Use SQLite".into(),
                body: "WAL mode".into(),
                status: RecordStatus::Active,
                pinned: false,
                source_session_id: None,
                created_at: now,
                updated_at: now,
                updated_by: "user".into(),
            })
            .unwrap();
        let m = store.modify_record(&r.id, |r| r.pinned = true).unwrap();
        assert!(m.pinned);
        assert!(matches!(
            store.modify_record(&r.id, |r| r.title.clear()),
            Err(StoreError::Invalid(_))
        ));
        store.delete_record(&r.id).unwrap();
        assert!(matches!(
            store.delete_record(&r.id),
            Err(StoreError::NotFound(_))
        ));

        let res = store
            .create_resource(Resource {
                id: crate::new_id(),
                project_id: "p".into(),
                kind: crate::model::ResourceKind::Link,
                url: "https://example.com".into(),
                title: "Ex".into(),
                meta: Some(json!({"a": 1})),
                created_at: now,
                updated_at: now,
                deleted: false,
            })
            .unwrap();
        assert_eq!(
            store.list_resources("p").unwrap()[0].meta,
            Some(json!({"a": 1}))
        );
        store
            .modify_resource(&res.id, |r| r.deleted = true)
            .unwrap();
        assert!(store.get_resource(&res.id).unwrap().is_none());
        assert!(store.list_resources("p").unwrap().is_empty());
    }
}
