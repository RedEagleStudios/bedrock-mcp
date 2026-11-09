//! Ore UI pages: the elements of the live Coherent Gameface views, read with a fixed script.
//!
//! Ore UI screens are Gameface views. Each is an `OreUI::View`, whose primary vtable is the
//! `OreUI::IView` interface (24 slots in LeviLamina's header) with `update(double)` in slot 4,
//! `getUrl()` in 13 and `getState()` in 17; its `cohtml::IViewListener` subobject at +0x18
//! (26 slots) has `OnReadyForBindings` in 7 and `OnBindingsReleased` in 8. LeviLamina puts the
//! view's `cohtml::View*` (`mGamefaceView`) first after the listener, at +0x20. The code
//! tolerates more virtuals and members between game versions: it accepts a `View` vtable with
//! more than 24 slots, and takes the first pointer in +0x20..+0x40 whose object has a vtable
//! outside the game's image (Gameface ships as its own module) with at least 69 slots, all of
//! them code.
//!
//! The public Gameface C++ API reference gives the order of `cohtml::View`'s methods. Its
//! absolute indices differ from the game's by a constant, so the code uses only the relative
//! positions: `RegisterForEvent`, `ExecuteScript(const char*)`, `AddInitialScript` and
//! `GetBinder` keep their documented spacing at slots 54, 62, 63 and 68. The probe below
//! confirms slot 62 on each game before the layer uses it for requests.
//!
//! The Ore UI library's log handler (`OreUI::Library::mLogHandler`, 2 slots; LeviLamina:
//! `OreUI::EmptyLogHandler`) is a `cohtml::Logging::ILogHandler`. The layer replaces its
//! `WriteLog(Severity, const char*, size_t)`, slot 1, in its vtable, and relies on the page's
//! console output reaching it; the probe checks that.
//!
//! The `update` hook runs on the thread the game drives the view from. After the original
//! returns, and only when a probe or a request waits, it checks the view (state painted,
//! bindings not released) and calls `ExecuteScript` with a fixed read-only script. The script
//! prints its JSON result with `console` under a nonce marker; the `WriteLog` hook takes the
//! marked lines out and passes every other message on. No lock is held across the script
//! call, so the code still works if the console output reaches `WriteLog` on the same thread
//! during the call.
//!
//! The capability stays pending until a probe script's nonce comes back from a live view.
//! If it never does, the layer stops calling slot 62 for the rest of the session.
//!
//! References: LeviLamina's `src-client/mc/client/gui/oreui/views/View.h`,
//! `oreui/interface/IView.h`, `oreui/interface/ViewState.h`, `oreui/Library.h`,
//! `oreui/EmptyLogHandler.h`, `mc/external/gameface/cohtml/IViewListener.h` and
//! `cohtml/ILogHandler.h`; the Gameface C++ API reference for `cohtml::View`
//! (docs.coherent-labs.com).

use std::{
    collections::HashMap,
    ffi::{CString, c_char, c_void},
    sync::{
        Condvar, LazyLock, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use endpoint::log;
use protocol::unstable::{CapabilityState, OreUiView};
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, VirtualQuery,
};

use crate::{capability, memory::Memory, vtable::Resolver};

const NAME: &str = "oreui";
const VIEW_CLASS: &str = "OreUI::View";
/// `OreUI::IView`'s slots in LeviLamina's header; the code accepts more.
const VIEW_SLOTS: usize = 24;
const LISTENER_OFFSET: usize = 0x18;
/// `cohtml::IViewListener`'s slots in LeviLamina's header.
const LISTENER_SLOTS: usize = 26;
/// The Ore UI library's log handler, by LeviLamina's name for it.
const LOG_CLASS: &str = "OreUI::EmptyLogHandler";
const LOG_SLOTS: usize = 2;

/// `OreUI::IView` slots.
const UPDATE: usize = 4;
const GET_URL: usize = 13;
const GET_STATE: usize = 17;
/// `cohtml::IViewListener` slots.
const ON_READY_FOR_BINDINGS: usize = 7;
const ON_BINDINGS_RELEASED: usize = 8;
/// `cohtml::Logging::ILogHandler::WriteLog`.
const WRITE_LOG: usize = 1;

/// Where to look for `OreUI::View::mGamefaceView`: LeviLamina puts it first after the
/// listener subobject, at +0x20; the code tolerates more members before it.
const VIEW_GAMEFACE: std::ops::Range<usize> = 0x20..0x40;
/// `cohtml::View::ExecuteScript`, and the vtable must hold code up to `GetBinder` (68): both
/// placed by their documented spacing from `RegisterForEvent` and `AddInitialScript`.
const EXECUTE_SCRIPT: usize = 62;
const GAMEFACE_SLOTS: usize = 69;

/// `OreUI::ViewState::ActiveFirstMeaningfulPaintHappened`: the page has loaded and drawn.
const PAINTED: u8 = 2;

/// Updates of painted views before the probe runs, so the page has settled.
const PROBE_SETTLE: u32 = 30;
/// View updates the probe waits for its answer.
const PROBE_FRAMES: u32 = 120;
/// Console methods the probe tries, in order of preference.
const METHODS: [&str; 4] = ["log", "info", "warn", "error"];
/// Chunk sizes the probe tries, largest first.
const CHUNKS: [usize; 2] = [4000, 1000];

/// How long a request waits for any view to update, and for all of them to answer.
const FIRST_UPDATE: Duration = Duration::from_millis(300);
const WAIT: Duration = Duration::from_millis(2000);
/// Bounds of one answer.
const MAX_PARTS: usize = 1024;
const MAX_ANSWER: usize = 2 << 20;
const MAX_URL: usize = 1024;

const MARKER: &[u8] = b"re_mcp:";
const END: &[u8] = b"|re_end";

/// The page script. Only `__NONCE__`, `__METHOD__` and `__CHUNK__` change. It reads the DOM
/// and writes nothing, catches every exception, and bounds the elements it scans and keeps,
/// their text and its output. Non-ASCII characters leave as `\u` escapes so a chunk can end
/// anywhere.
const SCRIPT: &str = r#"(function(){
var N="__NONCE__",M="__METHOD__",C=__CHUNK__,MAXK=1500,MAXS=12000,MAXC=2000,MAXT=256,MAXO=1048576;
function out(s){
s=s.replace(/[\u007f-\uffff]/g,function(c){return "\\u"+("000"+c.charCodeAt(0).toString(16)).slice(-4);});
if(s.length>MAXO)s=JSON.stringify({detail:"the page is larger than the output bound",truncated:true});
var n=Math.max(1,Math.ceil(s.length/C));
for(var i=0;i<n;i++)console[M]("re_mcp:"+N+":"+i+"/"+n+":"+s.substr(i*C,C)+"|re_end");
}
function cut(s){s=s.replace(/\s+/g," ").trim();return s.length>MAXT?s.slice(0,MAXT):s;}
function own(e){
var s="",c=e.firstChild,k=0;
while(c&&k<64&&s.length<4*MAXT){if(c.nodeType===3)s+=c.nodeValue;c=c.nextSibling;k++;}
return cut(s);
}
try{
var root=document.body||document.documentElement,act=document.activeElement;
var els=[],st=[root,-1],scanned=0,truncated=false;
while(st.length){
var p=st.pop(),e=st.pop();
if(scanned>=MAXS||els.length>=MAXK){truncated=true;break;}
scanned++;
var cs=getComputedStyle(e);
if(!cs||cs.display==="none"||cs.visibility==="hidden"||parseFloat(cs.opacity)===0)continue;
var tag=String(e.tagName).toLowerCase(),fid=e.getAttribute("data-focusable-id"),tid=e.getAttribute("data-testid"),role=e.getAttribute("role"),text=own(e);
var active=fid!==null||role!==null||tag==="button"||tag==="input"||tag==="textarea"||tag==="select"||tag==="a";
var oy=cs.overflowY||cs.overflow,ox=cs.overflowX||cs.overflow;
var scrolls=((oy==="auto"||oy==="scroll")&&e.scrollHeight>e.clientHeight+1)||((ox==="auto"||ox==="scroll")&&e.scrollWidth>e.clientWidth+1);
var me=p;
if(active||tid!==null||scrolls||text){
var b=e.getBoundingClientRect();
if(b.width>0&&b.height>0){
var o={parent:p<0?null:p,tag:tag,rect:[b.left,b.top,b.width,b.height]};
if(fid!==null)o.focus_id=fid;
if(tid!==null)o.test_id=tid;
if(role!==null)o.role=role;
if(tag==="input"&&e.type)o.input_type=String(e.type);
if(!text&&active){var l=e.getAttribute("aria-label")||e.getAttribute("title");if(l)text=cut(String(l));}
if(text)o.text=text;
if(e.getAttribute("aria-disabled")==="true"||e.disabled===true||e.hasAttribute("disabled")||(active&&cs.pointerEvents==="none"))o.disabled=true;
var ac=e.getAttribute("aria-checked");
if(ac==="true"||ac==="false")o.checked=ac==="true";
else if(tag==="input"&&(e.type==="checkbox"||e.type==="radio"))o.checked=!!e.checked;
if(scrolls)o.scrolls=true;
if(e===act)o.focused=true;
me=els.length;els.push(o);
}
}
var kids=e.children;
if(kids){var n=kids.length;if(n>MAXC){n=MAXC;truncated=true;}for(var k=n-1;k>=0;k--)st.push(kids[k],me);}
}
out(JSON.stringify({href:String(location.href),title:String(document.title||""),viewport:[innerWidth,innerHeight],elements:els,truncated:truncated}));
}catch(x){out(JSON.stringify({detail:"the page script failed: "+String(x)}));}
})();"#;

/// The probe: for each console method, one short line and one line of each chunk size.
const PROBE: &str = r#"(function(){
var N=[__NONCES__],M=["log","info","warn","error"],C=[4000,1000];
for(var m=0;m<M.length;m++){
console[M[m]]("re_mcp:"+N[m*3]+":0/1:probe|re_end");
for(var c=0;c<C.length;c++)console[M[m]]("re_mcp:"+N[m*3+1+c]+":0/1:"+new Array(C[c]+1).join("x")+"|re_end");
}
})();"#;

type Update = unsafe extern "system" fn(*mut c_void, f64);
type Listener = unsafe extern "system" fn(*mut c_void);
type WriteLog = unsafe extern "system" fn(*mut c_void, i32, *const c_char, usize);
type GetUrl = unsafe extern "system" fn(*mut c_void, *mut [usize; 2]) -> *mut [usize; 2];
type GetState = unsafe extern "system" fn(*mut c_void) -> u8;
type ExecuteScript = unsafe extern "system" fn(*mut c_void, *const c_char);

static ORIGINAL_UPDATE: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_READY: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_RELEASED: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_WRITE_LOG: AtomicUsize = AtomicUsize::new(0);
/// The `OreUI::View` primary vtable.
static VIEW_TABLE: AtomicUsize = AtomicUsize::new(0);
/// Addresses the game's image spans; Gameface's own vtables lie outside it.
static GAME_IMAGE: OnceLock<std::ops::Range<usize>> = OnceLock::new();

/// Nonzero while a probe or a request waits for the views: the update hook's fast path.
static WORK: AtomicU32 = AtomicU32::new(0);
/// Console method index and chunk size the probe found; the script uses them.
static METHOD: AtomicUsize = AtomicUsize::new(0);
static CHUNK: AtomicUsize = AtomicUsize::new(CHUNKS[1]);
static NONCES: AtomicU64 = AtomicU64::new(0);

/// Readiness per view address: set by `OnReadyForBindings`, cleared by `OnBindingsReleased`.
/// A view with no entry was ready before the layer loaded.
static READY: Mutex<Vec<(usize, bool)>> = Mutex::new(Vec::new());

/// The last script handed to `ExecuteScript`, kept alive until the next one.
static LAST_SCRIPT: Mutex<Option<CString>> = Mutex::new(None);
/// Serializes requests.
static REQUESTS: Mutex<()> = Mutex::new(());

enum Probe {
    /// Waiting for a painted view; counts its updates.
    Waiting(u32),
    /// The script ran; counts view updates since.
    Running {
        first: u64,
        updates: u32,
    },
    Done,
    /// Never call the script again.
    Off,
}

/// One marked message being put together from its chunks.
struct Answer {
    parts: Vec<Option<String>>,
    /// A chunk came without its end mark: the log cut it.
    cut: bool,
}

impl Answer {
    fn complete(&self) -> bool {
        !self.parts.is_empty() && self.parts.iter().all(Option::is_some)
    }
}

/// A view a request visited.
struct Visit {
    view: usize,
    url: String,
    state: u8,
    nonce: Option<u64>,
    detail: Option<String>,
    read_us: u64,
}

struct Job {
    generation: u64,
    url: Option<String>,
    visits: Vec<Visit>,
    /// A visited view updated again: every live view had its frame.
    cycled: bool,
}

struct State {
    probe: Probe,
    /// Answers by nonce, for the probe and the current job only.
    answers: HashMap<u64, Answer>,
    job: Option<Job>,
    generation: u64,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
    Mutex::new(State {
        probe: Probe::Off,
        answers: HashMap::new(),
        job: None,
        generation: 0,
    })
});
static CHANGED: Condvar = Condvar::new();

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|poison| poison.into_inner())
}

pub fn install(resolver: &Resolver<'_>) {
    let image = resolver.image;
    let end = image
        .sections
        .iter()
        .map(|section| section.rva as usize + section.bytes.len())
        .max()
        .unwrap_or(0);
    let _ = GAME_IMAGE.set(image.base as usize..image.base as usize + end);
    let result = (|| {
        let view = resolver.table_at_least(VIEW_CLASS, 0, VIEW_SLOTS)?;
        let listener = resolver.table(VIEW_CLASS, LISTENER_OFFSET as u32, LISTENER_SLOTS)?;
        let log_handler = resolver.table(LOG_CLASS, 0, LOG_SLOTS)?;
        capability::found(
            NAME,
            &[(VIEW_CLASS, view.found), (LOG_CLASS, log_handler.found)],
        );
        VIEW_TABLE.store(view.address, Ordering::Release);
        log_handler.hook(
            WRITE_LOG,
            write_log_hook as WriteLog as usize,
            &ORIGINAL_WRITE_LOG,
        )?;
        listener.hook(
            ON_READY_FOR_BINDINGS,
            ready_hook as Listener as usize,
            &ORIGINAL_READY,
        )?;
        listener.hook(
            ON_BINDINGS_RELEASED,
            released_hook as Listener as usize,
            &ORIGINAL_RELEASED,
        )?;
        state().probe = Probe::Waiting(0);
        WORK.fetch_add(1, Ordering::AcqRel);
        view.hook(UPDATE, update_hook as Update as usize, &ORIGINAL_UPDATE)
    })();
    match result {
        Ok(()) => capability::set(
            NAME,
            CapabilityState::Pending,
            "hooked; waiting for a painted Ore UI view to answer the probe script",
        ),
        Err(reason) => {
            state().probe = Probe::Off;
            capability::set(NAME, CapabilityState::Unavailable, reason);
        }
    }
}

fn set_ready(view: usize, ready: bool) {
    let mut all = READY.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(entry) = all.iter_mut().find(|(at, _)| *at == view) {
        entry.1 = ready;
    } else if all.len() < 256 {
        all.push((view, ready));
    }
}

unsafe extern "system" fn ready_hook(this: *mut c_void) {
    let original: Listener = unsafe { std::mem::transmute(ORIGINAL_READY.load(Ordering::Acquire)) };
    unsafe { original(this) };
    set_ready((this as usize).wrapping_sub(LISTENER_OFFSET), true);
}

unsafe extern "system" fn released_hook(this: *mut c_void) {
    set_ready((this as usize).wrapping_sub(LISTENER_OFFSET), false);
    let original: Listener =
        unsafe { std::mem::transmute(ORIGINAL_RELEASED.load(Ordering::Acquire)) };
    unsafe { original(this) };
}

unsafe extern "system" fn write_log_hook(
    this: *mut c_void,
    severity: i32,
    message: *const c_char,
    length: usize,
) {
    if !message.is_null() && length < (64 << 20) {
        // SAFETY: Gameface passes `length` bytes at `message`.
        let bytes = unsafe { std::slice::from_raw_parts(message.cast::<u8>(), length) };
        if take(bytes) {
            return;
        }
    }
    let original: WriteLog =
        unsafe { std::mem::transmute(ORIGINAL_WRITE_LOG.load(Ordering::Acquire)) };
    unsafe { original(this, severity, message, length) };
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// One marked chunk: nonce, index, count and text, and whether its end mark came with it.
fn parse(bytes: &[u8]) -> Option<(u64, usize, usize, &[u8], bool)> {
    let at = find(bytes, MARKER)?;
    let rest = &bytes[at + MARKER.len()..];
    let colon = rest.iter().position(|&byte| byte == b':')?;
    let nonce = u64::from_str_radix(std::str::from_utf8(&rest[..colon]).ok()?, 16).ok()?;
    let rest = &rest[colon + 1..];
    let slash = rest.iter().position(|&byte| byte == b'/')?;
    let index: usize = std::str::from_utf8(&rest[..slash]).ok()?.parse().ok()?;
    let rest = &rest[slash + 1..];
    let colon = rest.iter().position(|&byte| byte == b':')?;
    let count: usize = std::str::from_utf8(&rest[..colon]).ok()?.parse().ok()?;
    let text = &rest[colon + 1..];
    match text.windows(END.len()).rposition(|window| window == END) {
        Some(end) => Some((nonce, index, count, &text[..end], true)),
        None => Some((nonce, index, count, text, false)),
    }
}

/// Store a marked chunk; true when the message was ours and must not reach the game's log.
fn take(bytes: &[u8]) -> bool {
    if find(bytes, MARKER).is_none() {
        return false;
    }
    let Some((nonce, index, count, text, ended)) = parse(bytes) else {
        return true;
    };
    let mut state = state();
    let Some(answer) = state.answers.get_mut(&nonce) else {
        return true;
    };
    if count == 0 || count > MAX_PARTS || index >= count {
        return true;
    }
    if answer.parts.len() != count {
        answer.parts = vec![None; count];
    }
    answer.parts[index] = Some(String::from_utf8_lossy(text).into_owned());
    answer.cut |= !ended;
    let complete = answer.complete();
    drop(state);
    if complete {
        CHANGED.notify_all();
    }
    true
}

unsafe extern "system" fn update_hook(this: *mut c_void, time: f64) {
    let original: Update = unsafe { std::mem::transmute(ORIGINAL_UPDATE.load(Ordering::Acquire)) };
    unsafe { original(this, time) };
    if WORK.load(Ordering::Acquire) != 0 {
        // SAFETY: on the game thread, inside this view's own update.
        unsafe { serve(this as usize) };
    }
}

/// What the update hook learned about a view.
struct Seen {
    url: String,
    state: u8,
    /// Why the script cannot run on it now, if it cannot.
    blocked: Option<String>,
}

/// # Safety
/// `view` must be the `OreUI::View` whose `update` is running on this thread.
unsafe fn look(view: usize) -> Seen {
    let mut memory = Memory::default();
    let table = VIEW_TABLE.load(Ordering::Acquire);
    if memory.read::<usize>(view) != Some(table) {
        return Seen {
            url: String::new(),
            state: 0,
            blocked: Some("not an OreUI::View".into()),
        };
    }
    let slot = |index: usize| unsafe { *(table as *const usize).add(index) };
    let this = view as *mut c_void;
    let get_state: GetState = unsafe { std::mem::transmute(slot(GET_STATE)) };
    let state = unsafe { get_state(this) };
    let get_url: GetUrl = unsafe { std::mem::transmute(slot(GET_URL)) };
    let mut raw = [0usize; 2];
    unsafe { get_url(this, &mut raw) };
    let [data, length] = raw;
    let url = if length == 0 {
        String::new()
    } else if length <= MAX_URL && memory.readable(data, length) {
        // SAFETY: checked readable above.
        let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, length) };
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        return Seen {
            url: String::new(),
            state,
            blocked: Some("getUrl returned an implausible string".into()),
        };
    };
    let ready = READY
        .lock()
        .ok()
        .and_then(|all| all.iter().find(|(at, _)| *at == view).map(|entry| entry.1));
    let blocked = if state != PAINTED {
        Some(format!("the view is not painted (state {state})"))
    } else if ready == Some(false) {
        Some("the page released its bindings".into())
    } else {
        None
    };
    Seen {
        url,
        state,
        blocked,
    }
}

/// The view's `cohtml::View` and its `ExecuteScript`: the first member in [`VIEW_GAMEFACE`]
/// that points at an object whose vtable lies outside the game's image and holds
/// [`GAMEFACE_SLOTS`] slots of code.
fn gameface(view: usize) -> Result<(usize, ExecuteScript), String> {
    let mut memory = Memory::default();
    let image = GAME_IMAGE.get().ok_or("the game's image is not known")?;
    let (gameface, function) = VIEW_GAMEFACE
        .step_by(8)
        .find_map(|offset| {
            let gameface = memory
                .read::<usize>(view + offset)
                .filter(|&at| memory.object(at, 8))?;
            let table = memory
                .read::<usize>(gameface)
                .filter(|at| !image.contains(at))
                .filter(|&at| memory.object(at, GAMEFACE_SLOTS * 8))?;
            let slots = memory.read::<[usize; GAMEFACE_SLOTS]>(table)?;
            slots
                .iter()
                .all(|&slot| executable(slot))
                .then_some((gameface, slots[EXECUTE_SCRIPT]))
        })
        .ok_or_else(|| {
            format!(
                "no cohtml::View in OreUI::View +{:#x}..{:#x}: no member there points at an \
                 object with a vtable of {GAMEFACE_SLOTS} code slots outside the game's image",
                VIEW_GAMEFACE.start, VIEW_GAMEFACE.end
            )
        })?;
    // SAFETY: an executable slot of the view's own vtable.
    Ok((gameface, unsafe {
        std::mem::transmute::<usize, ExecuteScript>(function)
    }))
}

fn executable(address: usize) -> bool {
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: VirtualQuery only describes the address.
    let written = unsafe {
        VirtualQuery(
            address as *const c_void,
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    let code = PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;
    written != 0
        && info.State == MEM_COMMIT
        && info.Protect & code != 0
        && info.Protect & PAGE_GUARD == 0
}

/// Run `script` on the view; microseconds it took.
fn run(view: usize, script: String) -> Result<u64, String> {
    let (gameface, execute) = gameface(view)?;
    let script = CString::new(script).map_err(|_| "the script holds a NUL".to_owned())?;
    let start = Instant::now();
    // SAFETY: the game drives this view from this thread, inside its update; the script is
    // ours and NUL-terminated, and stays alive until the next one replaces it.
    unsafe { execute(gameface as *mut c_void, script.as_ptr()) };
    let took = start.elapsed().as_micros() as u64;
    *LAST_SCRIPT
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(script);
    Ok(took)
}

fn nonce() -> u64 {
    // Mix a counter with the clock so a stale line from an earlier load never matches.
    let count = NONCES.fetch_add(1, Ordering::AcqRel) + 1;
    let clock = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64);
    (clock << 16) ^ count
}

/// Lines the probe prints: per method, a short one and one per chunk size.
const PROBE_LINES: u64 = (METHODS.len() * (1 + CHUNKS.len())) as u64;

/// # Safety
/// On the game thread, inside `view`'s own update.
unsafe fn serve(view: usize) {
    let seen = unsafe { look(view) };
    let mut guard = state();
    let state = &mut *guard;
    match &mut state.probe {
        Probe::Off => {}
        Probe::Done => {
            drop(guard);
            serve_job(view, &seen);
        }
        Probe::Waiting(settled) => {
            if seen.blocked.is_some() {
                return;
            }
            *settled += 1;
            if *settled < PROBE_SETTLE {
                return;
            }
            let first = nonce();
            for offset in 0..PROBE_LINES {
                state.answers.insert(
                    first + offset,
                    Answer {
                        parts: Vec::new(),
                        cut: false,
                    },
                );
            }
            state.probe = Probe::Running { first, updates: 0 };
            drop(guard);
            let nonces: Vec<String> = (0..PROBE_LINES)
                .map(|offset| format!("\"{:016x}\"", first + offset))
                .collect();
            let script = PROBE.replace("__NONCES__", &nonces.join(","));
            log!("oreui: probing the view of {}", seen.url);
            if let Err(reason) = run(view, script) {
                finish_probe(Err(reason));
            }
        }
        Probe::Running { first, updates } => {
            *updates += 1;
            let (first, updates) = (*first, *updates);
            let arrived: Vec<(bool, bool)> = (0..PROBE_LINES)
                .map(|offset| {
                    state
                        .answers
                        .get(&(first + offset))
                        .map_or((false, false), |answer| (answer.complete(), answer.cut))
                })
                .collect();
            if !arrived.iter().all(|line| line.0) && updates < PROBE_FRAMES {
                return;
            }
            for offset in 0..PROBE_LINES {
                state.answers.remove(&(first + offset));
            }
            drop(guard);
            finish_probe(Ok(arrived));
        }
    }
}

/// Settle the probe from which of its lines arrived (complete, cut), per method: the short
/// line, then one line per chunk size.
fn finish_probe(arrived: Result<Vec<(bool, bool)>, String>) {
    let choice = arrived.as_ref().ok().and_then(|arrived| {
        METHODS.iter().enumerate().find_map(|(method, _)| {
            let base = method * 3;
            arrived[base].0.then(|| {
                let chunk = (0..CHUNKS.len())
                    .find(|&size| arrived[base + 1 + size] == (true, false))
                    .map(|size| CHUNKS[size]);
                (method, chunk)
            })
        })
    });
    let mut guard = state();
    match (choice, arrived) {
        (Some((method, Some(chunk))), _) => {
            METHOD.store(method, Ordering::Release);
            CHUNK.store(chunk, Ordering::Release);
            guard.probe = Probe::Done;
            drop(guard);
            WORK.fetch_sub(1, Ordering::AcqRel);
            capability::set(
                NAME,
                CapabilityState::Active,
                format!(
                    "the probe script's console.{} lines came back through the log handler",
                    METHODS[method]
                ),
            );
        }
        (Some((method, None)), _) => {
            guard.probe = Probe::Off;
            drop(guard);
            WORK.fetch_sub(1, Ordering::AcqRel);
            capability::set(
                NAME,
                CapabilityState::Failed,
                format!(
                    "console.{} lines reach the log handler, but the log cuts lines of {} characters",
                    METHODS[method], CHUNKS[1]
                ),
            );
        }
        (None, arrived) => {
            guard.probe = Probe::Off;
            drop(guard);
            WORK.fetch_sub(1, Ordering::AcqRel);
            let reason = match arrived {
                Err(reason) => reason,
                Ok(_) => "the probe script's console lines never reached the log handler's WriteLog: \
                          cohtml::View slot 62 may not be ExecuteScript on this build, or the page's \
                          console does not reach the log handler; slot 62 is not called again"
                    .into(),
            };
            capability::set(NAME, CapabilityState::Unavailable, reason);
        }
    }
}

/// Run the current request's script on `view`, once per request.
fn serve_job(view: usize, seen: &Seen) {
    let mut guard = state();
    let Some(job) = guard.job.as_mut() else {
        return;
    };
    if job.visits.iter().any(|visit| visit.view == view) {
        if !job.cycled {
            job.cycled = true;
            drop(guard);
            CHANGED.notify_all();
        }
        return;
    }
    let mut visit = Visit {
        view,
        url: seen.url.clone(),
        state: seen.state,
        nonce: None,
        detail: seen.blocked.clone(),
        read_us: 0,
    };
    if job
        .url
        .as_deref()
        .is_some_and(|wanted| !seen.url.contains(wanted))
    {
        // Not asked for: remember the visit so the cycle still completes.
        visit.detail = Some("skipped by the URL filter".into());
        job.visits.push(visit);
        return;
    }
    if visit.detail.is_some() {
        job.visits.push(visit);
        return;
    }
    let nonce = nonce();
    let generation = job.generation;
    visit.nonce = Some(nonce);
    job.visits.push(visit);
    guard.answers.insert(
        nonce,
        Answer {
            parts: Vec::new(),
            cut: false,
        },
    );
    drop(guard);
    let script = SCRIPT
        .replace("__NONCE__", &format!("{nonce:016x}"))
        .replace("__METHOD__", METHODS[METHOD.load(Ordering::Acquire)])
        .replace("__CHUNK__", &CHUNK.load(Ordering::Acquire).to_string());
    let result = run(view, script);
    let mut guard = state();
    if let Some(job) = guard
        .job
        .as_mut()
        .filter(|job| job.generation == generation)
        && let Some(visit) = job.visits.iter_mut().find(|visit| visit.view == view)
    {
        match result {
            Ok(took) => visit.read_us = took,
            Err(reason) => {
                visit.nonce = None;
                visit.detail = Some(reason);
            }
        }
    }
    drop(guard);
    CHANGED.notify_all();
}

/// Whether every visited view that ran the script has answered.
fn answered(state: &State, job: &Job) -> bool {
    job.visits.iter().all(|visit| {
        visit.nonce.is_none_or(|nonce| {
            state
                .answers
                .get(&nonce)
                .is_none_or(|answer| answer.complete())
        })
    })
}

/// The pages of the live views (whose URL contains `url`, when given), read on their next
/// frames.
pub fn read(url: Option<String>) -> Result<Vec<OreUiView>, String> {
    let _serial = REQUESTS.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut guard = state();
    match guard.probe {
        Probe::Done => {}
        Probe::Off => return Err("the oreui capability is off; see unstable_status".into()),
        _ => return Err("the oreui capability is still waiting for its probe".into()),
    }
    guard.generation += 1;
    guard.job = Some(Job {
        generation: guard.generation,
        url,
        visits: Vec::new(),
        cycled: false,
    });
    WORK.fetch_add(1, Ordering::AcqRel);
    let start = Instant::now();
    loop {
        let job = guard.job.as_ref().expect("this request's job");
        let elapsed = start.elapsed();
        let done = (job.cycled && answered(&guard, job))
            || (job.visits.is_empty() && elapsed >= FIRST_UPDATE)
            || elapsed >= WAIT;
        if done {
            break;
        }
        let limit = if job.visits.is_empty() {
            FIRST_UPDATE
        } else {
            WAIT
        };
        guard = CHANGED
            .wait_timeout(guard, limit.saturating_sub(elapsed))
            .map(|(guard, _)| guard)
            .unwrap_or_else(|poison| poison.into_inner().0);
    }
    WORK.fetch_sub(1, Ordering::AcqRel);
    let job = guard.job.take().expect("this request's job");
    let views = job
        .visits
        .into_iter()
        .filter(|visit| visit.detail.as_deref() != Some("skipped by the URL filter"))
        .map(|visit| {
            let answer = visit.nonce.and_then(|nonce| guard.answers.remove(&nonce));
            page(visit, answer)
        })
        .collect();
    guard.answers.clear();
    Ok(views)
}

fn page(visit: Visit, answer: Option<Answer>) -> OreUiView {
    let mut view = OreUiView {
        url: String::new(),
        href: String::new(),
        title: String::new(),
        state: 0,
        viewport: [0.0, 0.0],
        elements: Vec::new(),
        truncated: false,
        detail: visit.detail,
        read_us: 0,
    };
    if let Some(answer) = answer {
        if !answer.complete() {
            view.detail = Some("the page did not answer in time".into());
        } else if answer.cut {
            view.detail = Some("the log cut the page's answer".into());
        } else {
            let text: String = answer.parts.into_iter().flatten().collect();
            if text.len() > MAX_ANSWER {
                view.detail = Some("the page's answer is too large".into());
            } else {
                match serde_json::from_str::<OreUiView>(&text) {
                    Ok(parsed) => view = parsed,
                    Err(error) => {
                        view.detail = Some(format!("the page's answer is not valid: {error}"))
                    }
                }
            }
        }
    }
    view.url = visit.url;
    view.state = visit.state;
    view.read_us = visit.read_us;
    view
}
