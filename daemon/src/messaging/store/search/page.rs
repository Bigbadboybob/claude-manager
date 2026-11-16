use super::*;

pub(in crate::messaging::store) struct SearchPage<'a> {
    pub items: Vec<&'a Published>,
    pub scanned: Option<String>,
    pub truncated: bool,
}

impl Store {
    pub(in crate::messaging::store) fn search_page<'a>(
        &'a self,
        cids: &[String],
        search: &'a SearchFilter,
        anchor: Option<&ReadCursor>,
        newest: bool,
        after: Option<u64>,
        high: u64,
        limit: usize,
        matches: impl Fn(&&Published) -> bool,
    ) -> SearchPage<'a> {
        self.search_page_with_budget(
            cids,
            search,
            anchor,
            newest,
            after,
            high,
            limit,
            matches,
            10_000,
            Duration::from_millis(20),
        )
    }

    pub(super) fn search_page_with_budget<'a>(
        &'a self,
        cids: &[String],
        search: &'a SearchFilter,
        anchor: Option<&ReadCursor>,
        newest: bool,
        after: Option<u64>,
        high: u64,
        limit: usize,
        matches: impl Fn(&&Published) -> bool,
        visits: usize,
        duration: Duration,
    ) -> SearchPage<'a> {
        let start = Instant::now();
        let mut streams: Vec<_> = cids
            .iter()
            .map(|cid| {
                self.search_candidates(cid, search, anchor, newest, after, high)
                    .peekable()
            })
            .collect();
        let mut page = SearchPage {
            items: Vec::new(),
            scanned: None,
            truncated: false,
        };
        let mut visited = 0;
        loop {
            // Merge before filtering so a single global checkpoint can resume
            // even an empty page, without dropping hits in another conversation.
            let mut next: Option<(usize, &Published)> = None;
            for (n, stream) in streams.iter_mut().enumerate() {
                if let Some(&(e, _)) = stream.peek() {
                    let before = next.is_none_or(|(_, old)| {
                        let order = if after.is_some() {
                            e.position.cmp(&old.position)
                        } else {
                            let key = |e: &Published| {
                                strv(&e.event, "logical_time").parse::<u64>().unwrap_or(0)
                            };
                            key(e)
                                .cmp(&key(old))
                                .then_with(|| strv(&e.event, "id").cmp(strv(&old.event, "id")))
                        };
                        if newest {
                            order.is_gt()
                        } else {
                            order.is_lt()
                        }
                    });
                    if before {
                        next = Some((n, e));
                    }
                }
            }
            let Some((n, _)) = next else {
                break;
            };
            if visited > 0 && (visited >= visits || start.elapsed() >= duration) {
                page.truncated = true;
                break;
            }
            let (event, eligible) = streams[n].next().unwrap();
            visited += 1;
            #[cfg(test)]
            tests::visit();
            page.scanned = Some(strv(&event.event, "id").into());
            if eligible
                && matches(&event)
                && (search.words.len() <= 1
                    || words(strv(&event.event, "body"))
                        .windows(search.words.len())
                        .any(|w| w == search.words))
            {
                page.items.push(event);
                if page.items.len() > limit {
                    break;
                }
            }
        }
        page
    }
}
