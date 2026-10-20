# Deploy the Owner web board

Target: **cm-manager**, `board.latentoracle.xyz` → `35.186.186.160`, Cloudflare
proxied. This is a deployment recipe, not evidence that the service is deployed.
Swarm-Coord deploys the reviewed backend and frontend together; lanes do not.
B5b lists local and enrolled **cm-sessions** alerts through the existing authenticated
messaging connection. Deploy the brain changes on both hosts; older peers keep
local-only alert behavior. Channels, DMs and availability use the manager hub.

## Owner: Cloudflare setup

Do these steps in the account that owns `latentoracle.xyz`. Dashboard labels may
move; the named settings and required values below are the deployment contract.

1. In **Websites → latentoracle.xyz → DNS → Records → Add record**, choose `A`,
   name `board`, IPv4 `35.186.186.160`, TTL Auto, **Proxied / orange cloud**.
   Do not add an unproxied duplicate or an AAAA record for an unrelated host.
2. Open **Zero Trust → Settings → Custom Pages / Team domain** (also shown under
   the account's team settings). Record the team hostname, such as
   `your-team.cloudflareaccess.com`. Do not use `board.latentoracle.xyz` here.
3. Under **Zero Trust → Settings → Authentication → Login methods**, enable
   Google or One-time PIN. For Google, configure its OAuth credentials and
   Cloudflare callback as the dashboard instructs. PIN avoids that extra setup.
4. Under **Zero Trust → Access → Applications → Add an application → Self-hosted**,
   name it `CM Owner Board`. Add the public hostname `board.latentoracle.xyz`,
   covering **all paths**. Set application/session duration to **30 days** and
   select the login method from step 3. Some UIs offer session duration in the
   policy too; set that to 30 days as well. Do not enable an anonymous path,
   manifest bypass, service-auth bypass or bypass policy.
5. Add an **Allow** policy, name `Owner only`, **Include → Emails → Owner's exact
   email address**. Use a single email, not an email domain or Everyone. Remove
   any broader Allow/Bypass policies. Save the application. Access's default
   deny applies to everyone else.
6. Open the application's **Overview / Settings**, copy **Application Audience
   (AUD) Tag**, and give Swarm-Coord that value, the team hostname and the exact
   allowed email. These identify the expected signed assertion; do not paste an
   actual login token into chat. The origin independently checks RS256, issuer,
   audience, expiry and Owner email using the team's cached JWKS.
7. Under the zone's **SSL/TLS → Origin Server → Create Certificate**, choose
   Cloudflare-generated key and certificate, RSA, and hostname
   `board.latentoracle.xyz`. Choose the account's normal validity period. Save
   the **Origin Certificate** as `board-origin.pem` and its **Private Key** as
   `board-origin.key` through the secure operator transfer path. The key is
   displayed only once; keep it private, never in Git/chat. Swarm-Coord installs
   these under `/etc/nginx/ssl/`, key mode 0600 owned by root.
8. Under **SSL/TLS → Overview**, use **Full (strict)**. If the zone's other origins
   do not support strict mode, use a hostname-scoped Configuration Rule for
   `board.latentoracle.xyz` instead of breaking those origins. Under **SSL/TLS →
   Edge Certificates**, enable HTTPS redirect for this hostname (a hostname
   redirect rule is suitable). This redirect happens at Cloudflare; do not open
   origin port 80. Under **Caching → Cache Rules**, bypass cache for the entire
   board hostname. Leave Access in front of shell, icons, manifest, worker and
   API alike.

Confirm both identities: Owner can log in; a different email is denied. An
origin assertion must be from this exact Access application, not merely another
app on the same team. Cloudflare is the login UI; there is no local auth bypass.

## Coordinator: install the reviewed service

Python 3.11+ is required. Install nginx and a Python venv package if the host
lacks them. Review existing nginx listeners (`sudo nginx -T`) and GCP firewall
policies before changing them. Preserve all other services and SSH rules.

1. Deploy the reviewed daemon using the normal **brain-only** release procedure
   in [HOWTO_HOLDER_BRAIN_SPLIT.md](../HOWTO_HOLDER_BRAIN_SPLIT.md). This adds
   Operator-only `owner.notifications.next` and `owner_attention.list`, plus
   per-conversation `messaging.mark_read {mentions:true}`. It needs a manager
   brain restart, no holder upgrade, A-R, planning API restart or MCP payload
   change. B5b needs the origin/relay changes on both hosts (manager and sessions).
2. Stage the reviewed combined `web/` tree under `/opt/cm-board-web/web` and
   create `/opt/cm-board-web/OWNER.md` identifying the reviewed SHA, purpose,
   keep files and retirement condition. Install the Python environment:

   ```sh
   sudo python3 -m venv /opt/cm-board-web/.venv
   sudo /opt/cm-board-web/.venv/bin/pip install -r /opt/cm-board-web/web/requirements.txt
   sudo /opt/cm-board-web/.venv/bin/pip freeze > /path/to/private-release-evidence/requirements-resolved.txt
   sudo install -d -o lucas -g lucas -m 0700 /home/lucas/.cm/web-push
   ```

   Keep `/opt/cm-board-web` root-owned and readable/executable by lucas. The
   service writes only `~/.cm/web-push`. Home is hidden by a tmpfs; the sandbox
   binds only the scoped token, daemon socket and push directory. Code is
   read-only, memory is capped at 256 MB and tasks at 64. One worker is mandatory: the retry queue is a single-writer
   JSON checkpoint. Never run two service instances against the same state.
3. Copy [deploy/board.env.example](deploy/board.env.example) to
   `/etc/cm-board-web.env`, owner root:lucas, mode 0640, and replace all three
   Cloudflare placeholders. Check the configured daemon socket and token file
   belong to this host and are readable by lucas. Do not embed the token in the
   env file. First issue a scoped credential on manager using the **full**
   operator's normal CLI (not the web service):

   ```sh
   scripts/cm-op owner.board_token '{"action":"issue"}'
   ```

   This creates `~/.cm/board-token` mode 0600; the response contains its path,
   never its secret. `issue` is idempotent, `rotate` explicitly replaces it,
   and `revoke` deletes it. These management operations fail closed without
   configured strong operator authentication. Use `BOARD_TOKEN_FILE` in the
   service env, never the full operator token. The scoped credential permits
   only the board's messaging reads/sends/reactions/read marks, channel
   list/join, availability, alerts/seen and notification feed. It cannot spawn
   sessions, administer channels, manage tokens, restart the daemon or invoke
   arbitrary operator RPCs. A wrong/revoked scoped token never falls through
   the daemon's legacy operator compatibility mode.

   The adapter reads the token per call. **Restart cm-board-web after rotating
   its token or recreating the daemon socket**, since systemd binds those file
   inodes into its restricted mount namespace. Revocation takes effect at the
   daemon immediately; an old service instance cannot keep using a revoked
   token. Neither credentials nor push endpoints are HTTP outputs.
4. Install [deploy/cm-board-web.service](deploy/cm-board-web.service) to
   `/etc/systemd/system/cm-board-web.service`. Check with
   `systemd-analyze verify /etc/systemd/system/cm-board-web.service`, then run
   `sudo systemctl daemon-reload` and `sudo systemctl enable --now cm-board-web`.
   Verify `ss -ltn` shows **127.0.0.1:8010 only**, and:

   ```sh
   curl --fail http://127.0.0.1:8010/healthz
   curl -i http://127.0.0.1:8010/api/bootstrap
   ```

   The first returns `{"status":"ok"}`; the second must be **403**, including
   when given a forged `Cf-Access-Jwt-Assertion`. `/healthz` is liveness only;
   it does not attest daemon availability or successful delivery to an iPhone.
5. Generate firewall artifacts into a labelled private review directory:

   ```sh
   python3 web/deploy/cloudflare_firewall.py --project claude-manager-prod --network default > firewall.sh
   python3 web/deploy/cloudflare_firewall.py --format nginx > cm-board-cloudflare.conf
   ```

   The generator fetches Cloudflare's IPv4/IPv6 lists, runs a **read-only**
   `gcloud compute firewall-rules list` audit and prints a script. It never
   applies rules or installs anything. It fails if an enabled ingress allow
   for TCP 443/all protocols at priority ≤901 could override the restriction.
   `--rules-file exported-rules.json` supports offline review; regenerate with
   live rules just before applying. On `--update`, only the exact existing
   managed Cloudflare allow rules (restricted sources/target/port) are exempt.
   Review the output before running it. It creates separate v4/v6 allow rules
   at priority 900 and deny-other rules at 901, limited to TCP 443 and target tag
   `cm-board-origin`. Confirm the VPC name and audit **higher-priority existing
   rules and hierarchical firewall policies**: a broader prior allow must not
   override this restriction. Add the tag to **cm-manager only** with the
   correct project/zone; preserve its existing tags. No new rules for 80 or
   8010. If the rules already exist, review regenerated output with `--update`;
   that preserves each rule's existing ALLOW/DENY action, so verify those first.
6. Install the generated nginx include as
   `/etc/nginx/snippets/cm-board-cloudflare.conf`, and the cert/key at the paths
   in [deploy/nginx.conf](deploy/nginx.conf). Install/enable that virtual host
   after resolving existing listener conflicts. The allowlist evaluates the
   real TCP peer: inspect global `real_ip_header`/`set_real_ip_from` settings and
   do not let a global real-IP rewrite turn `$remote_addr` into the phone's IP.
   If needed, isolate this listener's real-IP configuration. Validate with
   `sudo nginx -t` before `sudo systemctl reload nginx`.
7. Verify the direct external origin on 443 is inaccessible from a non-Cloudflare
   network, 8010 is unreachable externally, and the proxied hostname goes
   through Access. Do not treat a local curl, Cloudflare 403, or TLS error as
   evidence of the wrong layer: inspect effective firewall rules and logs too.
   Rotate the allowlist after Cloudflare range changes: generate both artifacts,
   review their diff, apply the GCP updates and nginx include, `nginx -t`, reload.

The backend makes outbound HTTPS connections to the team's certs endpoint and
Apple/FCM/Mozilla/Windows push services. It follows no JWKS or push redirects.
There is no CORS or browser-selected RPC/actor. Writes require JSON and
`X-CM-Board: 1`; foreign Origin headers are rejected. Request bodies are limited
to 64 KiB. Script and style CSP are self-only; use frontend bddbef1 or newer.
Neither nginx nor uvicorn logs request URLs, JWTs or subscription bodies by
default. Do not enable debug/verbose Web Push logging in production.

## Device acceptance — remains open until a real iPhone is tested

Use **iOS 16.4+**. A headless desktop browser cannot establish this acceptance.

1. In Safari, open `https://board.latentoracle.xyz`, complete Access login as
   Owner, and verify lists and availability. Use **Share → Add to Home Screen**.
2. Close Safari and open the Home Screen icon. Confirm standalone layout and
   Access login continuity (or successful login inside that context). Check the
   manifest, icons and service worker loaded with Access credentials. A manifest
   login redirect is a failure, even if the Safari tab itself works.
3. Tap **Enable notifications** inside the installed app; allow the iOS prompt.
   Tap **Send test notification**, background the app and confirm a real system
   notification appears. Tap it; confirm the installed app opens.
4. At each Owner availability level (away, around, focused, on-call and unset),
   send a controlled DM and @mention from an agent and create a `notify_user`
   alert. Every level should deliver them. Confirm push title/short preview,
   conversation/alert tap target, and badge = unread DMs + mentions. Check all
   alert urgencies, including FYI. Verify explicit conversation mute/DND still
   suppresses chat pushes and read messages do not re-notify.
   Restore the previous availability and mark only test messages read.
5. Verify replies, reactions, pagination, per-channel clear mentions and stale
   seen acknowledgements. With B5b on both hosts, verify distinct host labels and
   that Seen clears the source alert after reconnect; legacy peers remain local-only.
   Confirm Telegram still follows its existing policy; web has no manifest
   viewer subscription and therefore does not suppress Telegram's no-viewer
   behavior. Do not assume every alert is supposed to create Telegram output.
6. Test Access session revocation/expiry from the installed app. Draft text must
   survive a sign-in/reload; API responses and conversation history must not be
   used as an offline auth bypass. Re-login must restore the app and its push
   registration. Test turning notifications off and a second device too.

If Home Screen login or authenticated manifest/worker fetching fails, record
iOS version, exact redirect/error and which context loses the cookie. Keep the
acceptance item open and propose **app-level Google sign-in** as a separately
reviewed fallback. Do not bypass Access for the shell/manifest/worker, weaken JWT
validation, or claim successful standalone login from a desktop-only check.

## Persistence, limits and operations

`~/.cm/web-push/` is 0700; `vapid.pem` and `state.json` are 0600. The service
generates its P-256 VAPID key once and refuses malformed existing keys. Back up
this directory privately and restore key + subscriptions together. Key rotation
requires devices to unsubscribe/re-subscribe; do not rotate during an upgrade.
`OWNER.md` describes retention. Subscriptions cap at 32 devices and retries at
2048 jobs; endpoints returning 404/410 are removed. 400/403/413 discard the
failed notification without retries. Temporary failures back off per endpoint
from 10 seconds up to 15 minutes, respecting a longer `Retry-After` (seconds or
HTTP date). Four endpoint requests can run concurrently, one message per
endpoint per sweep; the queue is saved once per sweep. Retry jobs expire one
hour after the decision. VAPID's public contact is the board origin, not Owner's
email. Explicit test pushes have a 30-second test rate limit.

The manager daemon retains at most 256 decisions / 512 KiB in
`~/.cm/owner-notifications.json`. Producers only enqueue to a bounded queue;
its writer does disk I/O outside the daemon/store locks. The web process
long-polls the feed (25s) and checkpoints its cursor with queued jobs. No message-history polling or second
availability filter. The hook reuses Owner inbox, mute/DND and read decisions;
it emits on the coordinator for replicated messages only after publication
durability, and does not replay the store's entire history on daemon startup.
Availability is informational: alerts, DMs and mentions deliver at every level.
Upgraded daemons release legacy held alerts without requiring a level change.
Previously suppressed chat is not replayed as catch-up pushes.

Delivery is **best effort**, not exactly-once: a push send accepted before a
checkpoint crash can repeat (stable notification tags replace duplicates).
Daemon death/storage failure between committing a message/alert and saving its
notification decision can lose that push; it never loses the source message.
A full producer queue drops the notification and increments the daemon health
counter. A replay overrun logs a warning and resumes retained events. A corrupt
or oversize checkpoint is renamed `.corrupt-<timestamp>-<uuid>` and replaced by
a new generation. `daemon.health.owner_notifications` reports queued/dropped
counts, the last error and recovery path. Keep quarantine evidence until the
incident is understood, then remove it. Malformed individual feed events are
logged/skipped without stalling the cursor. Decisions older than one hour expire. Recent queued notifications may be shown after a message
was read or an alert seen; the app always loads authoritative current state on
tap. Availability does not filter notification decisions or retries.

For incident checks use `systemctl status cm-board-web`,
`journalctl -u cm-board-web`, nginx error logs, and daemon notification checkpoint
errors. Test successful authenticated API access separately from health. No
HTTP endpoint lists subscriptions or returns keys other than the VAPID public
key. Access removal prevents new browser requests but does not revoke a device's
already registered push endpoint: remove the subscription in the authenticated
app, or stop the service and remove that endpoint and its queued jobs from the
private state file before restarting. Treat that file as sensitive.

To upgrade, stop only `cm-board-web`, replace the reviewed code/environment,
retain push state and restart it; nginx need not restart. For rollback, stop the
web service, restore the previous reviewed web release/venv, retain its matching
checkpoint schema and keys, then restart. If no working web release exists,
leave the service stopped; TUI/Telegram and agent sessions remain independent.
Use the holder/brain rollback procedure for a daemon regression. Do not restart
the holder or sessions to roll back the web frontend/backend.

## Local checks

Use a **private** Python venv (the checkout `.venv` may be a shared symlink):
install `web/requirements.txt` plus pytest and run
`PYTHONPATH=. python -m pytest -q web/tests`. Tests use temporary sockets, signed
test JWTs/mock JWKS and private push state. They never use the live daemon token
or send real push. Rust checks use `scripts/cm-test-isolated` with a private
target; run the Owner notification and messaging read-model/replication tests,
then `cargo build --workspace`. Delete disposable venvs/builds after validation;
keep logs under review with an OWNER.md and report their size at handoff.


## B5b alert relay operations

Both brains negotiate `owner_alerts_v1` on their existing paired messaging link.
No new listener, SSH session, remote operator credential or MCP payload is added.
Only active enrolled hosts with `owner_access` can relay snapshots or receive
Seen. Deploy both brains through their usual brain-only procedure; older peers
continue normal messaging without alert frames. Confirm `/api/bootstrap` lists
manager and sessions in `features.alert_hosts` after both connect. Source host
UUID and queue key are returned separately; web `key` is opaque and namespaced.

Each host keeps `~/.cm/owner-alert-relay.json` (private atomic checkpoint). It
holds a durable source snapshot generation/revision, plus (on the hub) cached
origin snapshots and pending exact-ID Seen requests. Bound: 16 origins, 4096
alerts / 600 KiB per snapshot, and a total checkpoint budget for 16 full
snapshots plus their full Seen maps and the local source (about 20 MiB).
Oversize or corrupt data retains prior state; the alert lane logs and backs
off 10–120 seconds while ordinary chat synchronization continues. Do not delete this file to clear an
alert: use Seen at the hub or the existing local acknowledgement. Restore it
with the host's matching identity/state during disaster recovery.

Source notify/withdraw/Seen/release signals the existing sync loop; unchanged
queues are reconciled every 10 seconds and on reconnect. Changed snapshots alone
are sent. Hub Seen also signals the peer push loop and survives an offline source.
One source Seen batch makes one queue write. Relay checkpoint writes hold neither
the daemon registry nor the messaging store lock. Cached alerts remain visible
offline and `alert_hosts[].connected` reports connection status. Revoking host
Owner access immediately excludes its cached rows and blocks Seen routing.

All pending source alerts enter snapshots regardless of availability.
Remote new IDs enqueue one hub Web Push decision after the snapshot is persisted;
replayed snapshots do not repush. This retains the core best-effort crash gap and
bounded feed behavior. A source alert created and removed entirely while offline
will not reach the hub. Telegram and per-host TUI manifests are unchanged. Legacy
alerts keep unknown creation times rather than receiving misleading new ages.


Alert snapshots now receive accepted/retry/resync results. A lower or conflicting
revision asks the source to persist a new generation; duplicate requests for an
old generation are harmless. Local snapshot failures, invalid incoming alerts,
Seen application failures and hub checkpoint errors are confined to the alert
lane and retry with capped backoff. An actual socket write failure still closes
the unusable shared connection. Pending Seen is resent until the source removes
its exact ID. Hub alert work runs on alert-version changes or bounded ten-second
reconciliation, not on each chat signal. Snapshot file reads happen after the
daemon registry lock is released. Removed Owner-authorized origins are pruned
when the hub lists or ingests alerts, freeing their cache slots.

On an origin's first ingest, pending alerts older than 15 minutes (or without a
known creation time) populate the list without sending a burst of old pushes.
Recent first-ingest alerts and new IDs on established origins retain normal
push behavior. Labels are capped at 4096 UTF-8 bytes at the source, including
legacy queued labels when projecting a snapshot. If alert metadata is unavailable,
bootstrap returns `features.alert_hosts: []` while chat and availability remain
usable; the dedicated alerts view still reports its own error.
