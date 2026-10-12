# CM Board frontend: local development

The frontend in `web/static/` is plain ES modules, CSS and a service worker:
no build step and no npm dependencies. It talks only to the API in
[`../API.md`](../API.md).

```bash
python3 web/dev/fake_server.py --port 8011   # in-memory backend implementing API.md
# open http://127.0.0.1:8011/  (any browser; narrow the window for the phone layout)
node --test web/dev/smoke.mjs                # helpers + shell files + API flows vs the fake
python3 web/dev/make_icons.py                # regenerate icons (pure Python)
```

The fake server enforces the same write headers as the real one
(`Content-Type: application/json`, `X-CM-Board: 1`) but has no Access check,
no daemon and no push delivery. Screenshots without a desktop browser:
`chrome-headless-shell --headless --window-size=390,844
--virtual-time-budget=4000 --screenshot=out.png http://127.0.0.1:8011/#alerts`.

Routes are hash based and match the push URLs: `#conversation=<id>&message=<id>`,
`#alerts&alert=<id>`, `#settings`. Drafts and their request IDs live in
localStorage, so a retry after a timeout or an Access re-login never posts twice.
