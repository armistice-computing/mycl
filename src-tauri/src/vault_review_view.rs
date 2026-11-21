//! Bounded UI report only. The immutable plan and source bytes stay in the owner.
use super::*;
use std::collections::{BTreeMap, BTreeSet};
impl Review {
    pub(crate) fn preview(&self, id: &str, expires: u64) -> Result<serde_json::Value, String> {
        let entries: BTreeMap<_, _> = self
            .plan
            .documents
            .iter()
            .map(|d| (d.id.as_str(), d.entry))
            .collect();
        let paired: BTreeSet<_> = self
            .notes
            .iter()
            .flat_map(|n| {
                n.inline_wires
                    .iter()
                    .map(|w| (entries[n.document_id.as_str()], w.source_offset))
            })
            .collect();
        let images: BTreeMap<_, _> = self
            .notes
            .iter()
            .flat_map(|n| {
                n.images.iter().map(|i| {
                    (
                        (entries[n.document_id.as_str()], n.body_start + i.start),
                        i.target_entry.is_some(),
                    )
                })
            })
            .collect();
        let mut supported = self.plan.tags.references.len();
        let mut unmapped = 0;
        let mut edges = BTreeSet::new();
        let mut seen = BTreeSet::new();
        for link in &self.plan.links {
            let key = (link.source, link.source_offset);
            seen.insert(key);
            if paired.contains(&key) || images.get(&key) == Some(&true) {
                supported += 1;
            } else if let Some(target) = link.whole_target() {
                supported += 1;
                edges.insert((link.source, target, "relatedTo".to_owned()));
            } else {
                unmapped += 1;
            }
        }
        for (key, mapped) in images {
            if !seen.contains(&key) {
                if mapped {
                    supported += 1;
                } else {
                    unmapped += 1;
                }
            }
        }
        for wire in &self.plan.export_wires {
            if wire.original_source_block.is_some() || wire.original_target_block.is_some() {
                unmapped += 1;
            } else {
                supported += 1;
                edges.insert((wire.source, wire.target, wire.predicate.clone()));
            }
        }
        unmapped += self
            .plan
            .issues
            .iter()
            .filter(|i| i.kind == "unresolved_export_wire")
            .count();
        let wire_count = self
            .notes
            .iter()
            .map(|n| n.inline_wires.len())
            .sum::<usize>()
            + self
                .plan
                .tags
                .references
                .iter()
                .filter(|t| t.range.is_none())
                .count()
            + edges.len();
        let mut warnings = vec![];
        let mut omitted = self.plan.tags.omitted_warnings;
        let mut push = |path: Option<&str>, message: &str| {
            if warnings.len() < 50 {
                warnings.push(json!({"path":path.map(|p|p.chars().take(1024).collect::<String>()),"message":message.chars().take(512).collect::<String>()}));
            } else {
                omitted += 1;
            }
        };
        for issue in &self.plan.issues {
            // A planning-time fragment candidate is not a final failure.
            if !matches!(
                issue.kind.as_str(),
                "fragment_reference" | "retained_reference"
            ) {
                push(Some(&issue.path), &issue.message);
            }
        }
        for message in &self.plan.tags.warnings {
            push(None, message);
        }
        for note in &self.notes {
            for message in &note.warnings {
                push(Some(&note.source_path), message);
            }
        }
        Ok(
            json!({"reviewId":id,"digest":self.digest,"destinationGraphId":crate::vault_job_bridge::destination(&self.source,&self.scan)?,"title":self.plan.folder_name,
            "expiresAt":expires,"counts":{"sourceNotes":self.plan.documents.len(),"generatedTagHubs":self.plan.tags.hubs.len(),"attachments":self.plan.attachments.len(),"folders":self.plan.folders.len(),"wires":wire_count},
            "warnings":warnings,"omittedWarnings":omitted,"supportedRefs":supported,"unmappedRefs":unmapped,"tagReferences":self.plan.tags.references.len(),"originalsPreserved":true}),
        )
    }
}
