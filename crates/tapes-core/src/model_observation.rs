use chrono::{DateTime, Utc};

use crate::model::{Model, ModelObservationStatus, ModelSelectionRecord, ModelSelectionSpan};

/// Streaming selection evidence keeps the newest 1024 spans and at most 32
/// distinct keys. Budget exhaustion makes attribution uncertain and withholds
/// the distinct count; native identities are capped at 4096 bytes each.
#[derive(Default)]
pub(crate) struct Selections {
    spans: Vec<ModelSelectionSpan>,
    keys: Vec<Model>,
    mixed: bool,
    uncertain: bool,
    exhausted: bool,
    interrupted: bool,
}

impl Selections {
    pub fn interrupt(&mut self) {
        self.uncertain = true;
        self.interrupted = true;
    }

    pub fn observe(&mut self, model: Model, timestamp: Option<DateTime<Utc>>, id: Option<&str>) {
        if model.id.len() > 4096 || model.variant.as_ref().is_some_and(|v| v.len() > 4096) {
            self.interrupt();
            self.exhausted = true;
            return;
        }
        if !self.keys.contains(&model) {
            self.mixed |= !self.keys.is_empty();
            if self.keys.len() < 32 {
                self.keys.push(model.clone());
            } else {
                self.exhausted = true;
            }
        }
        let native_id = id.filter(|id| id.len() <= 4096).map(str::to_owned);
        self.uncertain |= id.is_some() && native_id.is_none();
        let record = ModelSelectionRecord {
            timestamp,
            native_id,
        };
        if !self.interrupted {
            if let Some(span) = self.spans.last_mut().filter(|span| span.model == model) {
                span.last = record;
                return;
            }
        }
        self.interrupted = false;
        if self.spans.len() == 1024 {
            self.spans.remove(0);
            self.exhausted = true;
        }
        self.spans.push(ModelSelectionSpan {
            model,
            first: record.clone(),
            last: record,
        });
    }

    pub fn finish(
        self,
        head_read: bool,
    ) -> (Option<ModelObservationStatus>, Vec<ModelSelectionSpan>) {
        let status = (!self.spans.is_empty() || self.uncertain || !head_read).then_some(
            ModelObservationStatus {
                mixed: self.mixed,
                head_read,
                attribution_uncertain: self.uncertain || self.exhausted || !head_read,
                distinct_observed: (!self.exhausted).then_some(self.keys.len()),
            },
        );
        (status, self.spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(index: usize) -> Model {
        Model {
            id: format!("model-{index}"),
            variant: None,
        }
    }

    #[test]
    fn evidence_budgets_keep_the_newest_coordinate_and_withhold_exact_counts() {
        for distinct in [2, 40] {
            let mut selections = Selections::default();
            for index in 0..1100 {
                selections.observe(
                    model(index % distinct),
                    None,
                    Some(&format!("record-{index}")),
                );
            }
            let (status, spans) = selections.finish(true);
            let status = status.unwrap();
            assert!(status.mixed && status.attribution_uncertain && status.head_read);
            assert_eq!(status.distinct_observed, None);
            assert_eq!(spans.len(), 1024);
            assert_eq!(
                spans.last().unwrap().last.native_id.as_deref(),
                Some("record-1099")
            );
        }
    }

    #[test]
    fn unreadable_record_separates_equal_selections() {
        let mut selections = Selections::default();
        selections.observe(model(0), None, Some("before"));
        selections.interrupt();
        selections.observe(model(0), None, Some("after"));
        let (status, spans) = selections.finish(true);
        let status = status.unwrap();
        assert!(status.attribution_uncertain);
        assert!(!status.mixed);
        assert_eq!(status.distinct_observed, Some(1));
        assert_eq!(spans.len(), 2);
    }
}
