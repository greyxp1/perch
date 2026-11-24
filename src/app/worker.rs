use crate::document::{self, Animation, Input, Source, View, fit_view};
use crate::shm::Frame;
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    os::unix::{fs::MetadataExt, net::UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, LazyLock, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver},
    },
    time::Duration,
};

/// Bounds each speculative decode and all speculative sources and frames together.
const CACHE_BYTES: usize = 128 * 1024 * 1024;
pub(super) type Token = Arc<AtomicU64>;
pub(super) fn token() -> Token {
    Arc::new(AtomicU64::new(0))
}
pub(super) fn cancel(token: &Token) -> u64 {
    let generation = token.fetch_add(1, Ordering::Relaxed) + 1;
    // Locking first keeps a foreground wait on speculative work from missing the wakeup.
    drop(AHEAD.0.lock().unwrap());
    AHEAD.1.notify_all();
    generation
}
#[derive(Clone, Copy)]
pub(super) enum Step {
    Still,
    Next,
    Restart,
}
pub(super) enum Action {
    Open(Arc<crate::ipc::Request>),
    Load(Input),
    Render {
        source: Arc<Source>,
        view: View,
        step: Step,
    },
    Forget,
}
pub(super) struct Job {
    pub(super) id: u64,
    pub(super) generation: u64,
    pub(super) token: Token,
    pub(super) action: Action,
}
impl Job {
    fn live(&self) -> bool {
        self.token.load(Ordering::Relaxed) == self.generation
    }
    fn priority(&self) -> u8 {
        match self.action {
            Action::Forget => 0,
            Action::Render {
                step: Step::Next, ..
            } => 2,
            _ => 1,
        }
    }
}
pub(super) enum Output {
    Opened {
        sources: Vec<Arc<Source>>,
        paths: Arc<Vec<PathBuf>>,
    },
    Loaded(Arc<Source>, Option<(View, Arc<Frame>)>),
    Rendered {
        frame: Arc<Frame>,
        delay: Option<Duration>,
        stepped: bool,
    },
}
pub(super) struct Done {
    pub(super) id: u64,
    pub(super) generation: u64,
    pub(super) result: std::result::Result<Output, String>,
}
/// Upcoming paths of one pin, nearest first, and the screen they open on.
struct Plan {
    generation: u64,
    token: Token,
    paths: Vec<PathBuf>,
    /// Paths no worker took yet, and whether each retries without concurrent reservations.
    pending: Vec<(PathBuf, bool)>,
    screen: Screen,
}
impl Plan {
    fn live(&self) -> bool {
        self.token.load(Ordering::Relaxed) == self.generation
    }
}
type Screen = ((i32, i32), i32);
type Stamp = (u64, u64, i64, i64);
fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.ino(), meta.size(), meta.ctime(), meta.ctime_nsec()))
}
struct Entry {
    id: u64,
    source: Arc<Source>,
    stamp: Stamp,
    /// Absent when rendering would exceed the budget.
    frame: Option<(View, Arc<Frame>)>,
}
impl Entry {
    fn bytes(&self) -> usize {
        self.source.bytes() + self.frame.as_ref().map_or(0, |f| f.1.bytes())
    }
}
/// Speculative work on one path and the bytes it may still allocate.
#[derive(Clone)]
struct Claim {
    key: u64,
    id: u64,
    generation: u64,
    token: Token,
    path: PathBuf,
    bytes: usize,
}
impl Claim {
    fn live(&self) -> bool {
        self.token.load(Ordering::Relaxed) == self.generation
    }
}
/// Releases a claim and its reservation however its work ends.
struct Held(u64);
impl Drop for Held {
    fn drop(&mut self) {
        let (lock, ready) = &*AHEAD;
        (lock.lock().unwrap_or_else(PoisonError::into_inner).claims).retain(|c| c.key != self.0);
        ready.notify_all();
    }
}
#[derive(Default)]
struct Ahead {
    plans: HashMap<u64, Plan>,
    cache: Vec<Entry>,
    claims: Vec<Claim>,
    keys: u64,
}
impl Ahead {
    fn cached(&self) -> usize {
        self.cache.iter().map(Entry::bytes).sum()
    }
    /// The budget left beside the cache and every claim except `key`.
    fn free(&self, key: u64) -> usize {
        let reserved = (self.claims.iter())
            .filter(|c| c.key != key)
            .map(|c| c.bytes);
        CACHE_BYTES.saturating_sub(self.cached() + reserved.sum::<usize>())
    }
    fn reserve(&mut self, key: u64, bytes: usize) {
        if let Some(claim) = self.claims.iter_mut().find(|c| c.key == key) {
            claim.bytes = bytes;
        }
    }
    /// Claims the nearest pending path no live claim covers, reserving its decode budget.
    /// Returns the claim, its screen and whether sharing the budget lowered its limit.
    fn claim(&mut self, workers: usize) -> Option<(Held, Claim, Screen, bool)> {
        loop {
            let free = self.free(0);
            if free == 0 {
                return None;
            }
            let alone = self.claims.iter().all(|c| c.bytes == 0);
            let (id, index) = (self.plans.iter())
                .filter(|p| p.1.live())
                .find_map(|(&id, p)| {
                    if !alone && p.pending.first().is_some_and(|(_, retry)| *retry) {
                        return None;
                    }
                    let index = p.pending.iter().position(|(path, retry)| {
                        (alone || !retry)
                            && !(self.claims.iter()).any(|c| c.live() && &c.path == path)
                    });
                    index.map(|index| (id, index))
                })?;
            let plan = self.plans.get_mut(&id).unwrap();
            let (path, retry) = plan.pending.remove(index);
            let (token, generation, (bounds, scale)) =
                (plan.token.clone(), plan.generation, plan.screen);
            // A source cached without a frame had no room to render one.
            if (self.cache.iter()).any(|e| {
                e.source.path() == Some(&path)
                    && (e.frame.as_ref())
                        .is_none_or(|f| f.0 == fit_view(e.source.size, bounds, scale))
            }) {
                continue;
            }
            let headroom = CACHE_BYTES.saturating_sub(self.cached());
            let others = (self.plans.values()).any(|p| p.live() && !p.pending.is_empty());
            let bytes = if retry || !others {
                free
            } else {
                free.min(headroom / workers)
            };
            self.keys += 1;
            let claim = Claim {
                key: self.keys,
                id,
                generation,
                token,
                path,
                bytes,
            };
            self.claims.push(claim.clone());
            return Some((Held(claim.key), claim, (bounds, scale), bytes < headroom));
        }
    }
}
// Process-wide so cancellation can wake foreground loads waiting on speculative work.
static AHEAD: LazyLock<(Mutex<Ahead>, Condvar)> = LazyLock::new(Default::default);
/// Waits only for live jobs and live claims on the same path, and only reuses unchanged files.
fn take(input: &Input, live: impl Fn() -> bool) -> Option<Entry> {
    let path = input.path()?;
    let (lock, ready) = &*AHEAD;
    let mut ahead = lock.lock().unwrap();
    while (ahead.claims.iter()).any(|c| c.live() && c.path == path) && live() {
        ahead = ready.wait(ahead).unwrap();
    }
    if !live() {
        return None;
    }
    let index = (ahead.cache.iter()).position(|e| e.source.path() == Some(path))?;
    let entry = ahead.cache.swap_remove(index);
    ready.notify_all();
    drop(ahead);
    (stamp(path) == Some(entry.stamp)).then_some(entry)
}
/// Decodes and fits upcoming images within the shared budget.
fn lookahead(swap: bool, workers: usize) {
    let (lock, ready) = &*AHEAD;
    let mut ahead = lock.lock().unwrap();
    loop {
        let Some((held, claim, (bounds, scale), shared)) = ahead.claim(workers) else {
            ahead = ready.wait(ahead).unwrap();
            continue;
        };
        drop(ahead);
        let path = &claim.path;
        let cancelled = || !claim.live();
        let entry = (|| -> crate::Result<Entry> {
            // Stamping first makes a replacement during decoding look stale.
            let stamp = stamp(path).ok_or("cannot read image metadata")?;
            let source = Source::load(Input::Path(path.clone()), swap, claim.bytes, &cancelled)?;
            let view = fit_view(source.size, bounds, scale);
            let need = source.bytes() + source.fit_bytes(view);
            let render = {
                let mut ahead = lock.lock().unwrap();
                let render = need <= ahead.free(claim.key);
                let next = if render { need } else { source.bytes() };
                ahead.reserve(claim.key, next);
                if next < claim.bytes {
                    ready.notify_all();
                }
                render
            };
            let frame = render
                .then(|| source.render(view, None, swap, &cancelled))
                .transpose()?
                .map(|frame| (view, frame));
            Ok(Entry {
                id: claim.id,
                source,
                stamp,
                frame,
            })
        })();
        ahead = lock.lock().unwrap();
        let current =
            claim.live() && (ahead.plans.get(&claim.id)).is_some_and(|p| p.paths.contains(path));
        match entry {
            Ok(entry) if current => {
                ahead.cache.retain(|e| e.source.path() != Some(path));
                if entry.bytes() <= ahead.free(claim.key) {
                    ahead.cache.push(entry);
                }
                ahead.reserve(claim.key, 0);
            }
            // A budget shared with concurrent work may have rejected what a lone worker fits.
            Err(_) if current && shared => {
                let pending = &mut ahead.plans.get_mut(&claim.id).unwrap().pending;
                pending.retain(|p| &p.0 != path);
                pending.insert(0, (path.clone(), true));
            }
            _ => {}
        }
        drop(ahead);
        drop(held);
        ahead = lock.lock().unwrap();
    }
}
/// Applies the result of a task to the app on the main thread.
pub(super) type Apply = Box<dyn FnOnce(&mut super::App, &super::QueueHandle<super::App>) + Send>;
pub(super) struct Worker {
    queue: Arc<(Mutex<VecDeque<Job>>, Condvar)>,
    pub(super) wake: UnixStream,
    results: Receiver<Done>,
    notify: UnixStream,
    tasks: (mpsc::Sender<Apply>, Receiver<Apply>),
}
impl Worker {
    pub(super) fn new(swap: bool) -> std::io::Result<Self> {
        let (wake, mut notify) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        let task_notify = notify.try_clone()?;
        let (sender, results) = mpsc::sync_channel(2);
        let queue = Arc::new((Mutex::new(VecDeque::<Job>::new()), Condvar::new()));
        let work = queue.clone();
        std::thread::Builder::new()
            .name("perch-images".into())
            .spawn(move || {
                let mut animations = HashMap::<u64, Animation>::new();
                loop {
                    let (job, was_full) = {
                        let (lock, ready) = &*work;
                        let mut jobs = lock.lock().unwrap();
                        while jobs.is_empty() {
                            jobs = ready.wait(jobs).unwrap();
                        }
                        let index = jobs
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, j)| j.priority())
                            .unwrap()
                            .0;
                        let was_full = jobs.len() >= 32;
                        (jobs.remove(index).unwrap(), was_full)
                    };
                    // Skipped jobs have no result, but freeing queue space must wake retries.
                    if was_full && notify.write_all(&[1]).is_err() {
                        break;
                    }
                    if !job.live() {
                        continue;
                    }
                    if matches!(job.action, Action::Forget) {
                        animations.remove(&job.id);
                        continue;
                    }
                    let load = |input: &Input| {
                        if let Some(entry) = take(input, || job.live()) {
                            return Ok(Some((entry.source, entry.frame)));
                        }
                        if !job.live() {
                            return Ok(None);
                        }
                        Source::load(input.clone(), swap, usize::MAX, &|| !job.live())
                            .map(|source| Some((source, None)))
                    };
                    let result = (|| -> crate::Result<Option<Output>> {
                        match &job.action {
                            Action::Open(request) => {
                                let inputs = request.inputs()?;
                                let paths = if inputs.len() == 1 {
                                    inputs[0]
                                        .path()
                                        .map(document::neighbors)
                                        .unwrap_or_default()
                                } else {
                                    inputs
                                        .iter()
                                        .filter_map(|i| i.path().map(|p| p.to_path_buf()))
                                        .collect()
                                };
                                let mut sources = Vec::with_capacity(inputs.len());
                                for input in inputs.iter() {
                                    let Some((source, _)) = load(input)? else {
                                        return Ok(None);
                                    };
                                    sources.push(source);
                                }
                                Ok(Some(Output::Opened {
                                    sources,
                                    paths: Arc::new(paths),
                                }))
                            }
                            Action::Load(input) => {
                                animations.remove(&job.id);
                                let Some((source, frame)) = load(input)? else {
                                    return Ok(None);
                                };
                                Ok(Some(Output::Loaded(source, frame)))
                            }
                            Action::Render { source, view, step } => {
                                let stepped = !matches!(step, Step::Still);
                                let mut delay = None;
                                if stepped {
                                    let reset = matches!(step, Step::Restart)
                                        || animations
                                            .get(&job.id)
                                            .is_none_or(|a| !Arc::ptr_eq(&a.source, source));
                                    if reset {
                                        animations.remove(&job.id);
                                        animations.insert(job.id, Animation::new(source.clone())?);
                                    }
                                    delay = animations.get_mut(&job.id).unwrap().next(swap)?;
                                }
                                let current = animations
                                    .get(&job.id)
                                    .filter(|a| Arc::ptr_eq(&a.source, source))
                                    .and_then(|a| a.current.as_ref());
                                let frame = source.render(*view, current, swap, &|| !job.live())?;
                                Ok(Some(Output::Rendered {
                                    frame,
                                    delay,
                                    stepped,
                                }))
                            }
                            Action::Forget => unreachable!(),
                        }
                    })();
                    if !job.live() {
                        continue;
                    }
                    let Some(result) = result.map_err(|e| e.to_string()).transpose() else {
                        continue;
                    };
                    if sender
                        .send(Done {
                            id: job.id,
                            generation: job.generation,
                            result,
                        })
                        .is_err()
                        || notify.write_all(&[1]).is_err()
                    {
                        break;
                    }
                }
            })?;
        // Several cores leave room for a second speculative worker beside the foreground.
        let workers = match std::thread::available_parallelism().map_or(1, |n| n.get()) {
            4.. => 2,
            _ => 1,
        };
        for _ in 0..workers {
            (std::thread::Builder::new().name("perch-ahead".into()))
                .spawn(move || lookahead(swap, workers))?;
        }
        Ok(Self {
            queue,
            wake,
            results,
            notify: task_notify,
            tasks: mpsc::channel(),
        })
    }
    /// Runs an editing task on its own thread, keeping long encodes off the image worker.
    /// Returns false when no thread could start.
    pub(super) fn spawn(&self, task: impl FnOnce() -> Apply + Send + 'static) -> bool {
        let Ok(mut notify) = self.notify.try_clone() else {
            return false;
        };
        let sender = self.tasks.0.clone();
        let thread = std::thread::Builder::new().name("perch-edit".into());
        thread
            .spawn(move || {
                if sender.send(task()).is_ok() {
                    let _ = notify.write_all(&[1]);
                }
            })
            .is_ok()
    }
    /// Takes finished tasks after `drain` consumed their wakeups.
    pub(super) fn finished(&self) -> Vec<Apply> {
        self.tasks.1.try_iter().collect()
    }
    pub(super) fn submit(&self, job: Job) -> bool {
        let (lock, ready) = &*self.queue;
        let mut queue = lock.lock().unwrap();
        queue.retain(|old| old.id != job.id && old.live());
        if queue.len() >= 32 && !matches!(job.action, Action::Forget) {
            return false;
        }
        queue.push_back(job);
        ready.notify_one();
        true
    }
    /// Retargets speculative work for pin `id` and evicts its entries outside `paths`.
    pub(super) fn ahead(&self, id: u64, token: &Token, paths: Vec<PathBuf>, screen: Screen) {
        let (lock, ready) = &*AHEAD;
        let mut ahead = lock.lock().unwrap();
        let generation = token.load(Ordering::Relaxed);
        // Animation frames present without changing the plan, so its failures stay settled.
        if (ahead.plans.get(&id))
            .is_some_and(|p| (p.generation, &p.paths, p.screen) == (generation, &paths, screen))
        {
            return;
        }
        (ahead.cache).retain(|e| e.id != id || paths.iter().any(|p| e.source.path() == Some(p)));
        let plan = Plan {
            generation,
            token: token.clone(),
            pending: paths.iter().map(|p| (p.clone(), false)).collect(),
            paths,
            screen,
        };
        ahead.plans.insert(id, plan);
        ready.notify_all();
    }
    pub(super) fn drain(&self) -> Vec<Done> {
        let mut bytes = [0; 64];
        while (&self.wake).read(&mut bytes).is_ok_and(|n| n != 0) {}
        self.results.try_iter().collect()
    }
    pub(super) fn forget(&self, id: u64) {
        let mut ahead = AHEAD.0.lock().unwrap();
        ahead.plans.remove(&id);
        let before = ahead.cache.len();
        ahead.cache.retain(|e| e.id != id);
        if ahead.cache.len() < before {
            AHEAD.1.notify_all();
        }
        drop(ahead);
        let _ = self.submit(Job {
            id,
            generation: 0,
            token: token(),
            action: Action::Forget,
        });
    }
}
