//! Per-pane graphics state on the UI thread: maps the pane's image ids into
//! the outer terminal's shared id space, forwards rewritten commands, and
//! answers the program's queries itself.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};

use base64::Engine;

use super::command::Command;
use super::filter::{FilterEvent, Query, MAX_COMMAND_BYTES};

const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// One filtered event from the pane's stream. `replay` marks bytes from the
/// daemon's reattach replay: re-sent to the outer terminal, never answered.
#[derive(Debug)]
pub struct StreamEvent {
    pub event: FilterEvent,
    pub replay: bool,
}

/// A request to read the file behind a `t=f`/`t=t`/`t=s` transmission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    pub medium: char,
    pub path: String,
    pub offset: u64,
    pub size: u64,
}

pub type FetchResult = Result<Vec<u8>, String>;
pub type Fetcher = Box<dyn Fn(FetchRequest) -> Receiver<FetchResult> + Send>;

/// What the pane's replies need to know about the pane right now.
#[derive(Debug, Clone)]
pub struct PaneCtx<'a> {
    pub cols: u16,
    pub rows: u16,
    pub cell: Option<(u16, u16)>,
    pub version: &'a str,
}

/// Bytes produced by one [`PaneGraphics::pump`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    /// For the outer terminal.
    pub outer: Vec<u8>,
    /// Replies to write into the pane's PTY.
    pub pane: Vec<u8>,
}

/// Outer ids are shared by every pane; keep them within 24 bits so the
/// foreground colour alone encodes them.
static NEXT_OUTER_ID: AtomicU32 = AtomicU32::new(1);

fn alloc_outer_id() -> u32 {
    loop {
        let id = NEXT_OUTER_ID.fetch_add(1, Ordering::Relaxed) & 0x00ff_ffff;
        if id != 0 {
            return id;
        }
    }
}

/// How the program asked to be answered for one command.
#[derive(Debug, Clone)]
struct Reply {
    /// `i=…,I=…,p=…` with the program's own ids; empty = never answer.
    ids: String,
    quiet: u32,
    replay: bool,
}

impl Reply {
    fn render(&self, message: &str) -> Option<Vec<u8>> {
        let error = message != "OK";
        if self.replay || self.ids.is_empty() || self.quiet >= 2 || (self.quiet == 1 && !error) {
            return None;
        }
        Some(format!("\x1b_G{};{}\x1b\\", self.ids, message).into_bytes())
    }
}

enum Step {
    Outer(Vec<u8>),
    Pane(Vec<u8>),
    /// A file read in flight; later steps wait behind it to keep order.
    Fetch {
        rx: Receiver<FetchResult>,
        /// The rewritten command to forward with the file's bytes, or
        /// `None` for a query that only needs the answer.
        forward: Option<Command>,
        reply: Reply,
        success: &'static str,
    },
}

struct Upload {
    cmd: Command,
    replay: bool,
}

pub struct PaneGraphics {
    rx: Receiver<StreamEvent>,
    fetcher: Fetcher,
    /// Program image id → outer image id.
    ids: HashMap<u32, u32>,
    /// Program image number (`I=`) → program image id.
    numbers: HashMap<u32, u32>,
    upload: Option<Upload>,
    steps: VecDeque<Step>,
}

impl PaneGraphics {
    pub fn new(rx: Receiver<StreamEvent>, fetcher: Fetcher) -> Self {
        Self {
            rx,
            fetcher,
            ids: HashMap::new(),
            numbers: HashMap::new(),
            upload: None,
            steps: VecDeque::new(),
        }
    }

    /// Program image id → outer image id, for rewriting placeholder cells.
    pub fn ids(&self) -> &HashMap<u32, u32> {
        &self.ids
    }

    /// Process everything the stream has delivered and return what is ready
    /// to write, in order. Steps behind an unfinished file read stay queued.
    /// `ctx` is evaluated only when a query needs answering, so an idle
    /// pane costs one channel poll per tick.
    pub fn pump<'a>(&mut self, ctx: impl FnOnce() -> PaneCtx<'a>) -> Output {
        let mut ctx = Some(ctx);
        let mut resolved: Option<PaneCtx<'a>> = None;
        while let Ok(event) = self.rx.try_recv() {
            if matches!(event.event, FilterEvent::Query(_)) && resolved.is_none() {
                resolved = ctx.take().map(|f| f());
            }
            self.handle(event, resolved.as_ref());
        }
        let mut out = Output::default();
        while let Some(step) = self.steps.front_mut() {
            match step {
                Step::Outer(bytes) => out.outer.append(bytes),
                Step::Pane(bytes) => out.pane.append(bytes),
                Step::Fetch {
                    rx,
                    forward,
                    reply,
                    success,
                } => {
                    let result = match rx.try_recv() {
                        Ok(result) => result,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => Err("EIO:file read failed".into()),
                    };
                    let message = match result {
                        Ok(bytes) => {
                            if let Some(cmd) = forward.take() {
                                out.outer.extend(cmd.encode_chunked(BASE64.encode(bytes).as_bytes()));
                            }
                            success.to_string()
                        }
                        Err(e) => e,
                    };
                    if let Some(bytes) = reply.render(&message) {
                        out.pane.extend(bytes);
                    }
                }
            }
            self.steps.pop_front();
        }
        out
    }

    fn handle(&mut self, event: StreamEvent, ctx: Option<&PaneCtx>) {
        match event.event {
            FilterEvent::Query(query) => {
                if let (false, Some(ctx)) = (event.replay, ctx) {
                    self.steps.push_back(Step::Pane(query_reply(query, ctx)));
                }
            }
            FilterEvent::Graphics(body) => self.command(Command::parse(&body), event.replay),
        }
    }

    fn command(&mut self, mut cmd: Command, replay: bool) {
        // Chunks after the first carry only `m` (and `q`). Anything else
        // means the upload was abandoned (its program died mid-transfer):
        // drop it and handle the command normally.
        if self.upload.is_some() && !cmd.only_keys(b"mq") {
            self.upload = None;
        }
        if let Some(upload) = self.upload.as_mut() {
            upload.cmd.payload.append(&mut cmd.payload);
            if let Some(q) = cmd.get(b'q') {
                upload.cmd.set(b'q', q);
            }
            if cmd.get_u32(b'm') == Some(1) {
                if upload.cmd.payload.len() > MAX_COMMAND_BYTES {
                    self.upload = None;
                }
                return;
            }
            let upload = self.upload.take().expect("upload in progress");
            let mut cmd = upload.cmd;
            cmd.remove(b'm');
            return self.transmit(cmd, upload.replay);
        }
        match cmd.char(b'a').unwrap_or(b't') {
            b't' | b'T' | b'f' => {
                if cmd.get_u32(b'm') == Some(1) {
                    cmd.remove(b'm');
                    self.upload = Some(Upload { cmd, replay });
                } else {
                    self.transmit(cmd, replay);
                }
            }
            b'q' => self.query(cmd, replay),
            b'p' => self.put(cmd, replay),
            b'd' => self.delete(cmd),
            b'a' | b'c' => self.control(cmd, replay),
            _ => self.answer(&self.reply(&cmd, cmd.id(b'i'), replay), "EINVAL:unknown action"),
        }
    }

    fn reply(&self, cmd: &Command, image: Option<u32>, replay: bool) -> Reply {
        let mut ids = Vec::new();
        if let Some(i) = image {
            ids.push(format!("i={i}"));
        }
        if let Some(n) = cmd.id(b'I') {
            ids.push(format!("I={n}"));
        }
        if !ids.is_empty() {
            if let Some(p) = cmd.id(b'p') {
                ids.push(format!("p={p}"));
            }
        }
        Reply {
            ids: ids.join(","),
            quiet: cmd.get_u32(b'q').unwrap_or(0),
            replay,
        }
    }

    fn answer(&mut self, reply: &Reply, message: &str) {
        if let Some(bytes) = reply.render(message) {
            self.steps.push_back(Step::Pane(bytes));
        }
    }

    /// The program's image id a command refers to, by `i=` or `I=`.
    fn referenced_image(&self, cmd: &Command) -> Option<u32> {
        cmd.id(b'i')
            .or_else(|| cmd.id(b'I').and_then(|n| self.numbers.get(&n).copied()))
    }

    /// Rewrite ids for the outer terminal and silence its replies.
    fn outward(&self, cmd: &Command, outer: u32) -> Result<Command, &'static str> {
        let mut fwd = cmd.clone();
        fwd.set(b'i', outer);
        fwd.remove(b'I');
        fwd.set(b'q', 2);
        if let Some(parent) = cmd.id(b'P') {
            let parent = self.ids.get(&parent).ok_or("ENOPARENT:parent image not found")?;
            fwd.set(b'P', parent);
        }
        Ok(fwd)
    }

    fn transmit(&mut self, mut cmd: Command, replay: bool) {
        let action = cmd.char(b'a').unwrap_or(b't');
        let image = match (cmd.id(b'i'), cmd.id(b'I')) {
            (Some(i), _) => i,
            (None, Some(n)) if action != b'f' => {
                let mut i = alloc_outer_id();
                while self.ids.contains_key(&i) {
                    i = alloc_outer_id();
                }
                self.numbers.insert(n, i);
                i
            }
            (None, Some(n)) => match self.numbers.get(&n) {
                Some(&i) => i,
                None => return self.answer(&self.reply(&cmd, None, replay), "ENOENT:image not found"),
            },
            // Anonymous images can never be referenced by a placeholder.
            (None, None) => return,
        };
        let reply = self.reply(&cmd, Some(image), replay);
        let outer = if action == b'f' {
            match self.ids.get(&image) {
                Some(&outer) => outer,
                None => return self.answer(&reply, "ENOENT:image not found"),
            }
        } else {
            *self.ids.entry(image).or_insert_with(alloc_outer_id)
        };
        let payload = std::mem::take(&mut cmd.payload);
        let mut fwd = match self.outward(&cmd, outer) {
            Ok(fwd) => fwd,
            Err(e) => return self.answer(&reply, e),
        };
        let mut success = "OK";
        if action == b'T' && cmd.get(b'U') != Some("1") {
            // Direct placements are not drawn; keep the image for later
            // placeholder placements. See doc/kitty-graphics-passthrough.md.
            fwd.set(b'a', 't');
            success = DIRECT_PLACEMENT_UNSUPPORTED;
        }
        match fwd.char(b't').unwrap_or(b'd') {
            b'd' => {
                self.steps.push_back(Step::Outer(fwd.encode_chunked(&payload)));
                self.answer(&reply, success);
            }
            medium @ (b'f' | b't' | b's') => {
                let Some(request) = fetch_request(&fwd, medium, &payload) else {
                    return self.answer(&reply, "EINVAL:bad file path");
                };
                fwd.remove(b't');
                fwd.remove(b'O');
                fwd.remove(b'S');
                let rx = (self.fetcher)(request);
                self.steps.push_back(Step::Fetch {
                    rx,
                    forward: Some(fwd),
                    reply,
                    success,
                });
            }
            _ => self.answer(&reply, "EINVAL:unknown transmission medium"),
        }
    }

    fn query(&mut self, cmd: Command, replay: bool) {
        if replay {
            // Also skips reading (and deleting) a replayed temp file.
            return;
        }
        let reply = self.reply(&cmd, cmd.id(b'i'), replay);
        match cmd.char(b't').unwrap_or(b'd') {
            b'd' => self.answer(&reply, "OK"),
            medium @ (b'f' | b't' | b's') => match fetch_request(&cmd, medium, &cmd.payload) {
                Some(request) => {
                    let rx = (self.fetcher)(request);
                    self.steps.push_back(Step::Fetch {
                        rx,
                        forward: None,
                        reply,
                        success: "OK",
                    });
                }
                None => self.answer(&reply, "EINVAL:bad file path"),
            },
            _ => self.answer(&reply, "EINVAL:unknown transmission medium"),
        }
    }

    fn put(&mut self, cmd: Command, replay: bool) {
        let image = self.referenced_image(&cmd);
        let reply = self.reply(&cmd, image, replay);
        if cmd.get(b'U') != Some("1") {
            return self.answer(&reply, DIRECT_PLACEMENT_UNSUPPORTED);
        }
        let Some(&outer) = image.and_then(|i| self.ids.get(&i)) else {
            return self.answer(&reply, "ENOENT:image not found");
        };
        match self.outward(&cmd, outer) {
            Ok(fwd) => {
                self.steps.push_back(Step::Outer(fwd.encode()));
                self.answer(&reply, "OK");
            }
            Err(e) => self.answer(&reply, e),
        }
    }

    fn control(&mut self, cmd: Command, replay: bool) {
        let image = self.referenced_image(&cmd);
        let reply = self.reply(&cmd, image, replay);
        let Some(&outer) = image.and_then(|i| self.ids.get(&i)) else {
            return self.answer(&reply, "ENOENT:image not found");
        };
        match self.outward(&cmd, outer) {
            Ok(fwd) => {
                self.steps.push_back(Step::Outer(fwd.encode()));
                self.answer(&reply, "OK");
            }
            Err(e) => self.answer(&reply, e),
        }
    }

    fn delete(&mut self, cmd: Command) {
        let what = cmd.char(b'd').unwrap_or(b'a');
        let free = what.is_ascii_uppercase();
        let targets: Vec<u32> = match what.to_ascii_lowercase() {
            b'a' => self.ids.keys().copied().collect(),
            b'i' => cmd.id(b'i').into_iter().collect(),
            b'n' => cmd
                .id(b'I')
                .and_then(|n| self.numbers.get(&n).copied())
                .into_iter()
                .collect(),
            b'r' => {
                let (lo, hi) = (cmd.get_u32(b'x').unwrap_or(0), cmd.get_u32(b'y').unwrap_or(0));
                self.ids.keys().copied().filter(|i| (lo..=hi).contains(i)).collect()
            }
            // Positional deletes target direct placements, which are
            // never forwarded.
            _ => Vec::new(),
        };
        // A placement id narrows a single-image delete; `d=a` removes all.
        let placement = matches!(what.to_ascii_lowercase(), b'i' | b'n')
            .then(|| cmd.id(b'p'))
            .flatten();
        for image in targets {
            let Some(&outer) = self.ids.get(&image) else {
                continue;
            };
            self.steps.push_back(Step::Outer(delete_command(outer, free, placement)));
            if free && placement.is_none() {
                self.forget(image);
            }
        }
    }

    fn forget(&mut self, image: u32) {
        self.ids.remove(&image);
        self.numbers.retain(|_, i| *i != image);
    }
}

impl Drop for PaneGraphics {
    /// Free this pane's images in the outer terminal. Queued rather than
    /// written, because a pane can be dropped mid-frame.
    fn drop(&mut self) {
        let mut bytes = Vec::new();
        for &outer in self.ids.values() {
            bytes.extend(delete_command(outer, true, None));
        }
        super::outer::queue(&bytes);
    }
}

const DIRECT_PLACEMENT_UNSUPPORTED: &str =
    "ENOTSUP:claude-manager panes show images only through unicode placeholders (U=1)";

fn delete_command(outer: u32, free: bool, placement: Option<u32>) -> Vec<u8> {
    let mut cmd = Command::default();
    cmd.set(b'a', 'd');
    cmd.set(b'd', if free { 'I' } else { 'i' });
    cmd.set(b'i', outer);
    if let Some(p) = placement {
        cmd.set(b'p', p);
    }
    cmd.set(b'q', 2);
    cmd.encode()
}

fn fetch_request(cmd: &Command, medium: u8, payload: &[u8]) -> Option<FetchRequest> {
    let path = String::from_utf8(BASE64.decode(payload).ok()?).ok()?;
    if path.is_empty() {
        return None;
    }
    Some(FetchRequest {
        medium: medium as char,
        path,
        offset: cmd.get(b'O').and_then(|v| v.parse().ok()).unwrap_or(0),
        size: cmd.get(b'S').and_then(|v| v.parse().ok()).unwrap_or(0),
    })
}

fn query_reply(query: Query, ctx: &PaneCtx) -> Vec<u8> {
    let (cw, ch) = ctx.cell.unwrap_or((0, 0));
    match query {
        Query::CellSizePixels => format!("\x1b[6;{ch};{cw}t"),
        Query::TextAreaPixels => format!(
            "\x1b[4;{};{}t",
            ctx.rows as u32 * ch as u32,
            ctx.cols as u32 * cw as u32
        ),
        Query::XtVersion => format!("\x1bP>|{}\x1b\\", ctx.version),
    }
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    fn ctx() -> PaneCtx<'static> {
        PaneCtx {
            cols: 80,
            rows: 24,
            cell: Some((10, 20)),
            version: "kitty(0.39.1)",
        }
    }

    struct Harness {
        tx: mpsc::Sender<StreamEvent>,
        pane: PaneGraphics,
        fetches: Arc<Mutex<Vec<(FetchRequest, mpsc::Sender<FetchResult>)>>>,
    }

    impl Harness {
        fn new() -> Self {
            let (tx, rx) = mpsc::channel();
            let fetches: Arc<Mutex<Vec<(FetchRequest, mpsc::Sender<FetchResult>)>>> = Arc::default();
            let recorded = fetches.clone();
            let fetcher: Fetcher = Box::new(move |req| {
                let (tx, rx) = mpsc::channel();
                recorded.lock().unwrap().push((req, tx));
                rx
            });
            Self {
                tx,
                pane: PaneGraphics::new(rx, fetcher),
                fetches,
            }
        }

        fn send(&mut self, body: &str, replay: bool) -> Output {
            self.tx
                .send(StreamEvent {
                    event: FilterEvent::Graphics(body.as_bytes().to_vec()),
                    replay,
                })
                .unwrap();
            self.pane.pump(ctx)
        }

        fn outer_of(&self, image: u32) -> u32 {
            self.pane.ids()[&image]
        }
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn transmit_is_namespaced_silenced_and_answered_with_original_id() {
        let mut h = Harness::new();
        let out = h.send("a=T,U=1,i=7,f=100,c=4,r=2;QUJD", false);
        let outer = h.outer_of(7);
        assert_eq!(
            text(&out.outer),
            format!("\x1b_Ga=T,U=1,i={outer},f=100,c=4,r=2,q=2;QUJD\x1b\\")
        );
        assert_eq!(text(&out.pane), "\x1b_Gi=7;OK\x1b\\");
    }

    #[test]
    fn two_panes_using_the_same_id_get_distinct_outer_ids() {
        let mut a = Harness::new();
        let mut b = Harness::new();
        a.send("a=t,i=1,f=100,q=2;QUJD", false);
        b.send("a=t,i=1,f=100,q=2;QUJD", false);
        assert_ne!(a.outer_of(1), b.outer_of(1));
        assert!(a.outer_of(1) <= 0x00ff_ffff);
    }

    #[test]
    fn quiet_levels_are_honoured() {
        let mut h = Harness::new();
        assert!(h.send("a=t,i=1,q=1;QUJD", false).pane.is_empty());
        assert!(h.send("a=t,i=2,q=2;QUJD", false).pane.is_empty());
        // q=1 still reports errors.
        let err = h.send("a=p,U=1,i=99,q=1", false);
        assert!(text(&err.pane).starts_with("\x1b_Gi=99;ENOENT"));
    }

    #[test]
    fn chunked_upload_is_forwarded_whole_after_the_last_chunk() {
        let mut h = Harness::new();
        assert_eq!(h.send("a=t,i=5,f=100,m=1;AAAA", false), Output::default());
        assert_eq!(h.send("m=1;BBBB", false), Output::default());
        let out = h.send("m=0;CCCC", false);
        let outer = h.outer_of(5);
        assert_eq!(
            text(&out.outer),
            format!("\x1b_Ga=t,i={outer},f=100,q=2;AAAABBBBCCCC\x1b\\")
        );
        assert_eq!(text(&out.pane), "\x1b_Gi=5;OK\x1b\\");
    }

    #[test]
    fn an_abandoned_upload_does_not_swallow_the_next_command() {
        let mut h = Harness::new();
        h.send("a=t,i=5,f=100,m=1;AAAA", false);
        // The uploader died; a new program probes the terminal.
        let probe = h.send("a=q,i=31,t=d,s=1,v=1,f=24;AAAA", false);
        assert_eq!(text(&probe.pane), "\x1b_Gi=31;OK\x1b\\");
        assert!(!h.pane.ids().contains_key(&5));
        let fresh = h.send("a=t,i=6,q=2;QUJD", false);
        assert!(text(&fresh.outer).ends_with(";QUJD\x1b\\"));
    }

    #[test]
    fn image_numbers_get_a_fresh_id_that_later_puts_resolve() {
        let mut h = Harness::new();
        let out = h.send("a=t,I=3,f=100;QUJD", false);
        let reply = text(&out.pane);
        let image: u32 = reply
            .strip_prefix("\x1b_Gi=")
            .and_then(|r| r.split(',').next())
            .and_then(|v| v.parse().ok())
            .expect("reply names the assigned id");
        assert!(reply.contains(",I=3;OK"));
        let outer = h.outer_of(image);
        let put = h.send("a=p,U=1,I=3,p=9,q=2", false);
        assert_eq!(text(&put.outer), format!("\x1b_Ga=p,U=1,p=9,q=2,i={outer}\x1b\\"));
    }

    #[test]
    fn direct_placements_are_not_drawn() {
        let mut h = Harness::new();
        let out = h.send("a=T,i=4,f=100;QUJD", false);
        let outer = h.outer_of(4);
        assert_eq!(text(&out.outer), format!("\x1b_Ga=t,i={outer},f=100,q=2;QUJD\x1b\\"));
        assert!(text(&out.pane).starts_with("\x1b_Gi=4;ENOTSUP"));
        let put = h.send("a=p,i=4", false);
        assert!(put.outer.is_empty());
        assert!(text(&put.pane).starts_with("\x1b_Gi=4;ENOTSUP"));
    }

    #[test]
    fn deletes_are_rewritten_and_scoped_to_this_pane() {
        let mut h = Harness::new();
        h.send("a=t,i=1,q=2;QUJD", false);
        h.send("a=t,i=2,q=2;QUJD", false);
        let (o1, o2) = (h.outer_of(1), h.outer_of(2));
        let one = h.send("a=d,d=I,i=1", false);
        assert_eq!(text(&one.outer), format!("\x1b_Ga=d,d=I,i={o1},q=2\x1b\\"));
        assert!(!h.pane.ids().contains_key(&1));
        let all = h.send("a=d,d=a", false);
        assert_eq!(text(&all.outer), format!("\x1b_Ga=d,d=i,i={o2},q=2\x1b\\"));
        // Positional deletes never reach the outer terminal.
        assert!(h.send("a=d,d=p,x=1,y=1", false).outer.is_empty());
    }

    #[test]
    fn replay_is_forwarded_but_never_answered() {
        let mut h = Harness::new();
        let out = h.send("a=T,U=1,i=8;QUJD", true);
        assert!(!out.outer.is_empty());
        assert!(out.pane.is_empty());
        assert!(h.send("a=q,i=31,t=d,s=1,v=1,f=24;AAAA", true).pane.is_empty());
        h.tx.send(StreamEvent { event: FilterEvent::Query(Query::XtVersion), replay: true }).unwrap();
        assert!(h.pane.pump(ctx).pane.is_empty());
    }

    #[test]
    fn queries_are_answered_from_pane_geometry() {
        let mut h = Harness::new();
        for q in [Query::CellSizePixels, Query::TextAreaPixels, Query::XtVersion] {
            h.tx.send(StreamEvent { event: FilterEvent::Query(q), replay: false }).unwrap();
        }
        let out = h.pane.pump(ctx);
        assert_eq!(text(&out.pane), "\x1b[6;20;10t\x1b[4;480;800t\x1bP>|kitty(0.39.1)\x1b\\");
        let probe = h.send("a=q,i=31,t=d,s=1,v=1,f=24;AAAA", false);
        assert!(probe.outer.is_empty(), "queries are answered locally");
        assert_eq!(text(&probe.pane), "\x1b_Gi=31;OK\x1b\\");
    }

    #[test]
    fn file_transmission_is_fetched_and_sent_in_band_in_order() {
        let mut h = Harness::new();
        let path = BASE64.encode("/home/lucas/img.png");
        let first = h.send(&format!("a=t,t=f,i=6,f=100,S=3;{path}"), false);
        assert_eq!(first, Output::default(), "waits for the file");
        // A later command must not overtake the pending transmission.
        assert_eq!(h.send("a=p,U=1,i=6,q=2", false), Output::default());
        let (request, done) = h.fetches.lock().unwrap().pop().unwrap();
        assert_eq!(
            request,
            FetchRequest { medium: 'f', path: "/home/lucas/img.png".into(), offset: 0, size: 3 }
        );
        done.send(Ok(b"PNG".to_vec())).unwrap();
        let out = h.pane.pump(ctx);
        let outer = h.outer_of(6);
        assert_eq!(
            text(&out.outer),
            format!(
                "\x1b_Ga=t,i={outer},f=100,q=2;{}\x1b\\\x1b_Ga=p,U=1,i={outer},q=2\x1b\\",
                BASE64.encode("PNG")
            )
        );
        assert_eq!(text(&out.pane), "\x1b_Gi=6;OK\x1b\\");
    }

    #[test]
    fn failed_fetch_reports_the_error() {
        let mut h = Harness::new();
        let path = BASE64.encode("/nope.png");
        h.send(&format!("a=t,t=f,i=6;{path}"), false);
        let (_, done) = h.fetches.lock().unwrap().pop().unwrap();
        done.send(Err("ENOENT:/nope.png: missing".into())).unwrap();
        let out = h.pane.pump(ctx);
        assert!(out.outer.is_empty());
        assert_eq!(text(&out.pane), "\x1b_Gi=6;ENOENT:/nope.png: missing\x1b\\");
    }
}
