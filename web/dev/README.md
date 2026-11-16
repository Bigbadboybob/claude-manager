# CM Board frontend: local development

The frontend in `web/static/` is plain ES modules, CSS and a service worker:
no build step and no npm dependencies. It talks only to the API in
[`../API.md`](../API.md).

```bash
python3 web/dev/fake_server.py --port 8011   # in-memory backend implementing API.md
# open http://127.0.0.1:8011/  (any browser; narrow the window for the phone layout)
node --test web/dev/smoke.mjs                # helpers + shell files + API flows vs the fake
node --test web/dev/keyboard.mjs             # typing survives refreshes (headless Chromium, iPhone UA)
python3 web/dev/make_icons.py                # regenerate icons (pure Python)
```

The fake server enforces the same write headers as the real one
(`Content-Type: application/json`, `X-CM-Board: 1`) but has no Access check,
no daemon and no push delivery. Screenshots without a desktop browser:
`chrome-headless-shell --headless --window-size=390,844
--virtual-time-budget=4000 --screenshot=out.png http://127.0.0.1:8011/#alerts`.

`keyboard.mjs` drives Playwright's cached `chrome-headless-shell` (or
`CM_BOARD_CHROME`) over the DevTools protocol, with no npm packages. It loads
the app with `?refresh_ms=300` and types into each text field (channel and DM
composers, a reply, a new DM, the filter, New DM search) through several
refreshes, while agent messages arrive through the fake's dev-only
`POST /api/_dev/post`. It asserts that node, focus, value, caret and scroll
never change. Without a browser it skips; set `CM_BOARD_BROWSER_REQUIRED=1` to
make that a failure. It emulates an iPhone, but it is Blink, not WebKit.

Routes are hash based and match the push URLs: `#conversation=<id>&message=<id>`,
`#alerts&alert=<id>`, `#settings`. Drafts and their request IDs live in
localStorage, so a retry after a timeout or an Access re-login never posts twice.
