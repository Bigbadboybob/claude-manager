use super::*;

#[derive(Default)]
pub(in crate::messaging::store) struct SearchScope {
    pub ids: Vec<String>,
    pub partial: bool,
    pub indexing: bool,
}
impl Store {
    pub(in crate::messaging::store) fn search_scope(
        &mut self,
        actor: &str,
        conv: Option<&str>,
        public: bool,
        dms: bool,
        cursor: Option<&Value>,
    ) -> Result<SearchScope> {
        if self.read_model.search.budget == 0 {
            return Err(err("search_disabled", "Search byte budget is zero"));
        }
        let mut eligible: Vec<_> = self
            .read_model
            .conversations
            .iter()
            .filter(|(cid, _)| {
                conv.is_none_or(|id| id == cid.as_str())
                    && self.visible(actor, cid)
                    && (!public || self.channel_paths.contains_key(*cid))
                    && (!dms || self.conversations.contains_key(*cid))
            })
            .map(|(cid, c)| (cid.clone(), c.arrivals.last().map_or(0, |a| a.0)))
            .collect();
        eligible.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let total = eligible.len();
        let pinned = cursor
            .and_then(|c| c.get("search_scope"))
            .filter(|v| !v.is_null());
        let selected: Vec<String> = if let Some(ids) = pinned {
            let ids: Vec<String> = serde_json::from_value(ids.clone())
                .map_err(|_| err("invalid_cursor", "Invalid search scope"))?;
            if ids.len() > 20 || ids.iter().any(|id| !eligible.iter().any(|(e, _)| e == id)) {
                return Err(err("invalid_cursor", "Search scope is no longer visible"));
            }
            ids
        } else {
            eligible
                .into_iter()
                .take(if conv.is_some() { 1 } else { 20 })
                .map(|v| v.0)
                .collect()
        };
        let mut scope = SearchScope::default();
        let mut failed = None;
        for cid in selected {
            let search = &mut self.read_model.search;
            if let Some(error) = search.failure(&cid) {
                if conv.is_some() || pinned.is_some() {
                    return Err(error);
                }
                failed = Some(error);
            } else if search.conversations.contains_key(&cid) {
                search.touch(&cid);
                scope.ids.push(cid);
            } else {
                search.enqueue(cid, conv.is_some());
                scope.indexing = true;
            }
        }
        scope.partial =
            cursor.is_some_and(|c| c["search_partial_scope"] == true) || scope.ids.len() < total;
        // A new broad query can expose warm conversations while its bounded
        // cold selection builds. Continuations pin only that exposed scope.
        if scope.indexing && (scope.ids.is_empty() || conv.is_some() || pinned.is_some()) {
            return Err(err(
                "search_indexing",
                "Search index is building in the background; retry the same query after 1 second",
            ));
        }
        if scope.ids.is_empty() {
            if let Some(error) = failed {
                return Err(error);
            }
        }
        Ok(scope)
    }
}
