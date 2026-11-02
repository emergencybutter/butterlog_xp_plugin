//! The telemetry link to the ButterLog app (sim-native telemetry design §4.2, rollout
//! phase 4): every frame, the datarefs the app asked for, read on the flight loop and sent
//! over localhost TCP with the sim clock.
//!
//! The plugin only reads. The app quantises, samples, chunks, spools and uploads, with the
//! same sampler it uses for MSFS, so rates and quanta live in one place and the plugin doesn't
//! change when they do.
//!
//! Threads: a network thread accepts one app at a time on `127.0.0.1:PORT`, sends `Hello`,
//! and passes the app's `Subscribe` to the flight loop, which resolves the datarefs (only the
//! main thread may) and from then on reads them every frame into a fixed-size frame, handed
//! back over a bounded channel. The flight loop neither allocates nor does I/O, and never
//! waits: when the channel is full the frame is dropped and counted. When the app goes away,
//! the network thread drops its end and the flight loop stops reading.
//!
//! Messages, both ways: `u32` length of what follows, then a `u8` type and the payload, all
//! little-endian; strings are a `u16` byte length and UTF-8.
//!
//! - plugin → app `HELLO`: protocol `u16`, plugin version string, X-Plane version `i32`.
//! - app → plugin `SUBSCRIBE`: count `u16`, then per value: dataref name string and element
//!   `u16` (`SCALAR` for a scalar dataref).
//! - plugin → app `SUBSCRIBED`: count `u16`, then per value `u8` 1 if found, 0 if not.
//! - plugin → app `FRAME`: sim time `f64` (`sim/time/total_running_time_sec`), wall time
//!   `i64` ms since the Unix epoch, frames dropped so far `u32`, count `u16`, then one `f64`
//!   per subscribed value in order (0 for one not found).
//! - plugin → app `EVENT`: kind `u8` (`EVENT_*`), detail string.

use std::ffi::CString;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const PORT: u16 = 49021;
pub const PROTOCOL: u16 = 1;

pub const HELLO: u8 = 1;
pub const SUBSCRIBED: u8 = 2;
pub const FRAME: u8 = 3;
pub const EVENT: u8 = 4;
pub const SUBSCRIBE: u8 = 1;

/// The user's aircraft was loaded; detail is its `.acf` path.
pub const EVENT_AIRCRAFT_LOADED: u8 = 1;
/// A flight started at an airport (`XPLM_MSG_AIRPORT_LOADED`).
pub const EVENT_AIRPORT_LOADED: u8 = 2;

pub const SCALAR: u16 = u16::MAX;
/// Values one frame can carry.
pub const MAX_VALUES: usize = 256;
/// About 17 s at 60 fps before frames are dropped.
const QUEUE: usize = 1024;
/// Anything longer from the app is not a subscription.
const MAX_MESSAGE: usize = 1 << 20;
const SIM_TIME: &str = "sim/time/total_running_time_sec";

pub struct Frame {
    pub sim_time: f64,
    pub wall_ms: i64,
    pub dropped: u32,
    pub n: u16,
    pub values: [f64; MAX_VALUES],
}

pub enum Out {
    Subscribed(Vec<u8>),
    Frame(Box<Frame>),
    Event(u8, String),
}

/// What the app asked for: dataref name and element.
pub type Entry = (String, u16);

struct Subscription {
    tx: SyncSender<Out>,
    slots: Vec<Slot>,
    dropped: u32,
    /// Reused frame buffers: each sent frame comes back through `spare` once written, so the
    /// flight loop doesn't allocate.
    spare: Receiver<Box<Frame>>,
}

#[derive(Clone, Copy)]
enum Reader {
    Double,
    Float,
    Int,
    FloatElement(i32),
    IntElement(i32),
    Missing,
}

struct Slot {
    dataref: xplm_sys::XPLMDataRef,
    read: Reader,
}

// --- encoding, shared by both directions -------------------------------------------------

pub fn put_str(out: &mut Vec<u8>, s: &str) {
    let bytes = &s.as_bytes()[..s.len().min(u16::MAX as usize)];
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// Wraps a payload as a message of `kind`.
pub fn message(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.extend_from_slice(&(payload.len() as u32 + 1).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    out
}

pub fn hello(plugin_version: &str, xplane_version: i32) -> Vec<u8> {
    let mut p = PROTOCOL.to_le_bytes().to_vec();
    put_str(&mut p, plugin_version);
    p.extend_from_slice(&xplane_version.to_le_bytes());
    message(HELLO, &p)
}

pub fn encode(out: &Out) -> Vec<u8> {
    match out {
        Out::Subscribed(found) => {
            let mut p = (found.len() as u16).to_le_bytes().to_vec();
            p.extend_from_slice(found);
            message(SUBSCRIBED, &p)
        }
        Out::Frame(f) => {
            let n = f.n as usize;
            let mut p = Vec::with_capacity(22 + 8 * n);
            p.extend_from_slice(&f.sim_time.to_le_bytes());
            p.extend_from_slice(&f.wall_ms.to_le_bytes());
            p.extend_from_slice(&f.dropped.to_le_bytes());
            p.extend_from_slice(&f.n.to_le_bytes());
            for v in &f.values[..n] {
                p.extend_from_slice(&v.to_le_bytes());
            }
            message(FRAME, &p)
        }
        Out::Event(kind, detail) => {
            let mut p = vec![*kind];
            put_str(&mut p, detail);
            message(EVENT, &p)
        }
    }
}

struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u16()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
}

/// A `SUBSCRIBE` payload (after the type byte).
pub fn decode_subscribe(payload: &[u8]) -> Option<Vec<Entry>> {
    let mut c = Cursor(payload);
    let n = c.u16()? as usize;
    let mut entries = Vec::with_capacity(n);
    for _ in 0..n {
        entries.push((c.str()?, c.u16()?));
    }
    c.0.is_empty().then_some(entries)
}

// --- the flight loop's side -----------------------------------------------------------------

fn find(name: &str) -> xplm_sys::XPLMDataRef {
    match CString::new(name) {
        Ok(c) => unsafe { xplm_sys::XPLMFindDataRef(c.as_ptr()) },
        Err(_) => std::ptr::null_mut(),
    }
}

/// How to read a dataref by the type it reports, or `Missing`.
fn resolve(name: &str, element: u16) -> Slot {
    let dataref = find(name);
    if dataref.is_null() {
        return Slot { dataref, read: Reader::Missing };
    }
    let types = unsafe { xplm_sys::XPLMGetDataRefTypes(dataref) } as u32;
    let has = |t: u32| types & t != 0;
    let read = if element == SCALAR {
        if has(xplm_sys::xplmType_Double) {
            Reader::Double
        } else if has(xplm_sys::xplmType_Float) {
            Reader::Float
        } else if has(xplm_sys::xplmType_Int) {
            Reader::Int
        } else if has(xplm_sys::xplmType_FloatArray) {
            Reader::FloatElement(0)
        } else if has(xplm_sys::xplmType_IntArray) {
            Reader::IntElement(0)
        } else {
            Reader::Missing
        }
    } else {
        let i = i32::from(element);
        let size = |vf: bool| unsafe {
            if vf {
                xplm_sys::XPLMGetDatavf(dataref, std::ptr::null_mut(), 0, 0)
            } else {
                xplm_sys::XPLMGetDatavi(dataref, std::ptr::null_mut(), 0, 0)
            }
        };
        if has(xplm_sys::xplmType_FloatArray) && i < size(true) {
            Reader::FloatElement(i)
        } else if has(xplm_sys::xplmType_IntArray) && i < size(false) {
            Reader::IntElement(i)
        } else {
            Reader::Missing
        }
    };
    Slot { dataref, read }
}

fn read(slot: &Slot) -> f64 {
    let d = slot.dataref;
    unsafe {
        match slot.read {
            Reader::Double => xplm_sys::XPLMGetDatad(d),
            Reader::Float => xplm_sys::XPLMGetDataf(d).into(),
            Reader::Int => xplm_sys::XPLMGetDatai(d).into(),
            Reader::FloatElement(i) => {
                let mut v = 0f32;
                xplm_sys::XPLMGetDatavf(d, &mut v, i, 1);
                v.into()
            }
            Reader::IntElement(i) => {
                let mut v = 0i32;
                xplm_sys::XPLMGetDatavi(d, &mut v, i, 1);
                v.into()
            }
            Reader::Missing => 0.0,
        }
    }
}

fn wall_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// A subscription request from the network thread: what to read, where to send it, and the
/// channel the written frames come back on for reuse.
pub struct Request {
    entries: Vec<Entry>,
    tx: SyncSender<Out>,
    spare: Receiver<Box<Frame>>,
}

/// Lives in the plugin state; `sample` runs on every flight loop.
pub struct Link {
    requests: Receiver<Request>,
    subscription: Option<Subscription>,
    sim_time: Slot,
    /// The network thread's log lines, printed from the main thread: the XPLM API isn't
    /// for other threads.
    logs: Receiver<String>,
    log: fn(&str),
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Link {
    /// Starts listening. Main thread only (it resolves the sim clock).
    pub fn start(plugin_version: &'static str, log: fn(&str)) -> Option<Self> {
        let listener = match TcpListener::bind(("127.0.0.1", PORT)) {
            Ok(l) => l,
            Err(e) => {
                log(&format!("Telemetry link: cannot listen on 127.0.0.1:{PORT}: {e}"));
                return None;
            }
        };
        if listener.set_nonblocking(true).is_err() {
            return None;
        }
        let mut xplane_version = 0;
        let mut xplm_version = 0;
        let mut host_id = 0;
        unsafe { xplm_sys::XPLMGetVersions(&mut xplane_version, &mut xplm_version, &mut host_id) };
        let (requests_tx, requests) = mpsc::channel();
        let (logs_tx, logs) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("butterlog-link".into())
                .spawn(move || serve(listener, requests_tx, stop, plugin_version, xplane_version, logs_tx))
                .ok()?
        };
        log(&format!("Telemetry link: listening on 127.0.0.1:{PORT}"));
        Some(Self {
            requests,
            subscription: None,
            sim_time: resolve(SIM_TIME, SCALAR),
            logs,
            log,
            stop,
            thread: Some(thread),
        })
    }

    /// Takes a new subscription if the app sent one, then reads this frame's values.
    pub fn sample(&mut self) {
        while let Ok(line) = self.logs.try_recv() {
            (self.log)(&line);
        }
        match self.requests.try_recv() {
            Ok(request) => self.subscribe(request),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.subscription = None,
        }
        let Some(sub) = &mut self.subscription else { return };
        let Ok(mut frame) = sub.spare.try_recv() else {
            // Every buffer is queued or being written: the app is behind.
            sub.dropped = sub.dropped.saturating_add(1);
            return;
        };
        frame.sim_time = read(&self.sim_time);
        frame.wall_ms = wall_ms();
        frame.dropped = sub.dropped;
        frame.n = sub.slots.len() as u16;
        for (v, slot) in frame.values.iter_mut().zip(&sub.slots) {
            *v = read(slot);
        }
        match sub.tx.try_send(Out::Frame(frame)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => sub.dropped = sub.dropped.saturating_add(1),
            Err(TrySendError::Disconnected(_)) => self.subscription = None,
        }
    }

    fn subscribe(&mut self, request: Request) {
        let entries = &request.entries[..request.entries.len().min(MAX_VALUES)];
        let slots: Vec<Slot> = entries.iter().map(|(name, element)| resolve(name, *element)).collect();
        let found = slots.iter().map(|s| u8::from(!matches!(s.read, Reader::Missing))).collect();
        if request.tx.try_send(Out::Subscribed(found)).is_err() {
            return;
        }
        self.subscription = Some(Subscription { tx: request.tx, slots, dropped: 0, spare: request.spare });
    }

    /// Sent in order with the frames, when an app is subscribed.
    pub fn event(&mut self, kind: u8, detail: String) {
        if let Some(sub) = &self.subscription {
            if let Err(TrySendError::Disconnected(_)) = sub.tx.try_send(Out::Event(kind, detail)) {
                self.subscription = None;
            }
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.subscription = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// --- the network thread ---------------------------------------------------------------------

fn serve(
    listener: TcpListener,
    requests: Sender<Request>,
    stop: Arc<AtomicBool>,
    plugin_version: &'static str,
    xplane_version: i32,
    logs: Sender<String>,
) {
    let log = |line: String| {
        let _ = logs.send(line);
    };
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                log(format!("Telemetry link: app connected from {peer}"));
                let reason = match client(stream, &requests, &stop, plugin_version, xplane_version) {
                    Ok(()) => "closed".to_string(),
                    Err(e) => e.to_string(),
                };
                log(format!("Telemetry link: app disconnected ({reason})"));
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => {
                log(format!("Telemetry link: accept failed: {e}"));
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// Reads whole messages from a non-blocking stream.
#[derive(Default)]
pub struct Inbox {
    buf: Vec<u8>,
}

impl Inbox {
    /// Appends what's available. `Ok(false)` once the peer closed.
    pub fn fill(&mut self, stream: &mut impl Read) -> std::io::Result<bool> {
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => return Ok(false),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => return Ok(true),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// The next whole message as (type, payload).
    pub fn next(&mut self) -> std::io::Result<Option<(u8, Vec<u8>)>> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(self.buf[..4].try_into().unwrap()) as usize;
        if len == 0 || len > MAX_MESSAGE {
            return Err(std::io::Error::new(ErrorKind::InvalidData, format!("message of {len} bytes")));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let kind = self.buf[4];
        let payload = self.buf[5..4 + len].to_vec();
        self.buf.drain(..4 + len);
        Ok(Some((kind, payload)))
    }
}

fn client(
    mut stream: TcpStream,
    requests: &Sender<Request>,
    stop: &AtomicBool,
    plugin_version: &str,
    xplane_version: i32,
) -> std::io::Result<()> {
    // One non-blocking socket for both directions. (A clone shares the socket, so making
    // a clone blocking for writes made reads block too, and the subscription never got
    // past `Inbox::fill`.)
    stream.set_nonblocking(true)?;
    stream.set_nodelay(true)?;
    write_all(&mut stream, &hello(plugin_version, xplane_version))?;

    let mut inbox = Inbox::default();
    // Frames from the flight loop, for the current subscription.
    let mut frames: Option<(Receiver<Out>, SyncSender<Box<Frame>>)> = None;
    while !stop.load(Ordering::Relaxed) {
        if !inbox.fill(&mut stream)? {
            return Ok(());
        }
        while let Some((kind, payload)) = inbox.next()? {
            if kind != SUBSCRIBE {
                continue;
            }
            let entries = decode_subscribe(&payload)
                .ok_or_else(|| std::io::Error::new(ErrorKind::InvalidData, "bad subscription"))?;
            let (tx, rx) = mpsc::sync_channel(QUEUE);
            let (spare_tx, spare) = mpsc::sync_channel(QUEUE);
            for _ in 0..QUEUE {
                let _ = spare_tx.try_send(Box::new(Frame { sim_time: 0.0, wall_ms: 0, dropped: 0, n: 0, values: [0.0; MAX_VALUES] }));
            }
            if requests.send(Request { entries, tx, spare }).is_err() {
                return Ok(());
            }
            frames = Some((rx, spare_tx));
        }
        let Some((rx, spare)) = &frames else {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(out) => {
                let mut batch = encode(&out);
                recycle(out, spare);
                // Whatever else is queued, in one write.
                while let Ok(out) = rx.try_recv() {
                    batch.extend_from_slice(&encode(&out));
                    recycle(out, spare);
                    if batch.len() > 1 << 16 {
                        break;
                    }
                }
                write_all(&mut stream, &batch)?;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => frames = None,
        }
    }
    Ok(())
}

/// How long the app may leave a write unread before the link is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// `write_all` for a non-blocking stream: retries until everything is written or
/// [`WRITE_TIMEOUT`] passes without progress.
fn write_all(stream: &mut impl Write, mut buf: &[u8]) -> std::io::Result<()> {
    let mut last_progress = std::time::Instant::now();
    while !buf.is_empty() {
        match stream.write(buf) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = &buf[n..];
                last_progress = std::time::Instant::now();
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                if last_progress.elapsed() > WRITE_TIMEOUT {
                    return Err(ErrorKind::TimedOut.into());
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn recycle(out: Out, spare: &SyncSender<Box<Frame>>) {
    if let Out::Frame(frame) = out {
        let _ = spare.try_send(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The app's side (`butterlog-app` `telemetry/xp_plugin.rs`) has the same vectors.
    #[test]
    fn golden_messages() {
        assert_eq!(
            hello("0.3.0", 121_400),
            [14, 0, 0, 0, HELLO, 1, 0, 5, 0, b'0', b'.', b'3', b'.', b'0', 0x38, 0xDA, 0x01, 0x00]
        );
        let mut values = [0.0; MAX_VALUES];
        values[0] = 1.5;
        let frame = Out::Frame(Box::new(Frame { sim_time: 2.0, wall_ms: 3, dropped: 4, n: 1, values }));
        let mut expected = vec![31, 0, 0, 0, FRAME];
        expected.extend_from_slice(&2.0f64.to_le_bytes());
        expected.extend_from_slice(&3i64.to_le_bytes());
        expected.extend_from_slice(&[4, 0, 0, 0, 1, 0]);
        expected.extend_from_slice(&1.5f64.to_le_bytes());
        assert_eq!(encode(&frame), expected);
        assert_eq!(
            encode(&Out::Event(EVENT_AIRCRAFT_LOADED, "a.acf".into())),
            [9, 0, 0, 0, EVENT, 1, 5, 0, b'a', b'.', b'a', b'c', b'f']
        );
        assert_eq!(encode(&Out::Subscribed(vec![1, 0])), [5, 0, 0, 0, SUBSCRIBED, 2, 0, 1, 0]);
    }

    #[test]
    fn subscriptions_decode() {
        // As the app encodes one: two values, the second an array element.
        let mut p = 2u16.to_le_bytes().to_vec();
        put_str(&mut p, "sim/a");
        p.extend_from_slice(&SCALAR.to_le_bytes());
        put_str(&mut p, "sim/b");
        p.extend_from_slice(&1u16.to_le_bytes());
        assert_eq!(decode_subscribe(&p), Some(vec![("sim/a".into(), SCALAR), ("sim/b".into(), 1)]));
        assert_eq!(decode_subscribe(&p[..p.len() - 1]), None, "truncated");
    }

    #[test]
    fn the_inbox_splits_messages_across_reads() {
        let mut bytes = message(SUBSCRIBE, &[1, 2, 3]);
        bytes.extend(message(SUBSCRIBE, &[4]));
        let mut inbox = Inbox::default();
        let mut first = &bytes[..6];
        assert!(inbox.fill(&mut first).is_ok());
        assert_eq!(inbox.next().unwrap(), None);
        let mut rest = &bytes[6..];
        inbox.fill(&mut rest).unwrap();
        assert_eq!(inbox.next().unwrap(), Some((SUBSCRIBE, vec![1, 2, 3])));
        assert_eq!(inbox.next().unwrap(), Some((SUBSCRIBE, vec![4])));
        assert_eq!(inbox.next().unwrap(), None);
    }

    /// The network thread against an app on a loopback port: hello, subscription, the flight
    /// loop's answer and a frame, then the app closing.
    #[test]
    fn a_client_subscribes_and_gets_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (requests_tx, requests) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            client(stream, &requests_tx, &AtomicBool::new(false), "0.3.1", 121_400)
        });

        let mut app = TcpStream::connect(addr).unwrap();
        app.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let read_message = |app: &mut TcpStream| {
            let mut len = [0u8; 4];
            app.read_exact(&mut len).unwrap();
            let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
            app.read_exact(&mut body).unwrap();
            body
        };
        assert_eq!(read_message(&mut app)[0], HELLO);
        let mut p = 1u16.to_le_bytes().to_vec();
        put_str(&mut p, "sim/a");
        p.extend_from_slice(&SCALAR.to_le_bytes());
        app.write_all(&message(SUBSCRIBE, &p)).unwrap();

        // The flight loop's part.
        let request: Request = requests.recv_timeout(Duration::from_secs(5)).expect("subscription passed on");
        assert_eq!(request.entries, vec![("sim/a".to_string(), SCALAR)]);
        request.tx.send(Out::Subscribed(vec![1])).unwrap();
        let mut frame = request.spare.recv().unwrap();
        frame.sim_time = 7.0;
        frame.n = 1;
        frame.values[0] = 42.0;
        request.tx.send(Out::Frame(frame)).unwrap();

        assert_eq!(read_message(&mut app), [SUBSCRIBED, 1, 0, 1]);
        let body = read_message(&mut app);
        assert_eq!(body[0], FRAME);
        assert_eq!(f64::from_le_bytes(body[1..9].try_into().unwrap()), 7.0);
        assert_eq!(f64::from_le_bytes(body[body.len() - 8..].try_into().unwrap()), 42.0);

        drop(app);
        assert!(server.join().unwrap().is_ok(), "ends cleanly when the app closes");
    }

    #[test]
    fn oversized_messages_are_refused() {
        let mut inbox = Inbox::default();
        let mut bytes = &u32::MAX.to_le_bytes()[..];
        inbox.fill(&mut bytes).unwrap();
        assert!(inbox.next().is_err());
    }
}
