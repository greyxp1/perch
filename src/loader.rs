use crate::document::{self, Animation, Input, Source, View};
use crate::image::Frame;
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    os::unix::{fs::MetadataExt, net::UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver},
    },
    time::Duration,
};

/// Bounds each speculative decode and all speculative sources and frames together.
const CACHE_BYTES: usize = 128 * 1024 * 1024;
pub type Token = Arc<AtomicU64>;
pub fn token() -> Token {
    Arc::new(AtomicU64::new(0))
}
pub fn cancel(token: &Token) -> u64 {
    let generation = token.fetch_add(1, Ordering::Relaxed) + 1;
    // Locking first keeps a foreground wait on speculative work from missing the wakeup.
    drop(AHEAD.0.lock().unwrap());
    AHEAD.1.notify_all();
    generation
}
#[derive(Clone, Copy)]
pub enum Step {
    Still,
    Next,
    Restart,
}
pub enum Action {
    Open(Arc<crate::ipc::Request>),
    Load(Input),
    Render {
        source: Arc<Source>,
        view: View,
        step: Step,
    },
    Forget,
}
pub struct Job {
    pub id: u64,
    pub generation: u64,
    pub token: Token,
    pub action: Action,
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
pub enum Output {
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
pub struct Done {
    pub id: u64,
    pub generation: u64,
    pub result: std::result::Result<Output, String>,
}
/// Upcoming paths of one pin, nearest first, and the screen they open on.
struct Plan {
    generation: u64,
    token: Token,
    paths: Vec<PathBuf>,
    next: usize,
    screen: ((i32, i32), i32),
}
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
#[derive(Default)]
struct Ahead {
    plans: HashMap<u64, Plan>,
    cache: Vec<Entry>,
    busy: Option<PathBuf>,
}
// Process-wide so cancellation can wake foreground loads waiting on speculative work.
static AHEAD: LazyLock<(Mutex<Ahead>, Condvar)> = LazyLock::new(Default::default);
/// Waits only for live jobs and speculative work on the same path, and only reuses unchanged files.
fn take(input: &Input, live: impl Fn() -> bool) -> Option<Entry> {
    let path = input.path()?;
    let (lock, ready) = &*AHEAD;
    let mut ahead = lock.lock().unwrap();
    while ahead.busy.as_deref() == Some(path) && live() {
        ahead = ready.wait(ahead).unwrap();
    }
    if !live() {
        return None;
    }
    let index = (ahead.cache.iter()).position(|e| e.source.path() == Some(path))?;
    let entry = ahead.cache.swap_remove(index);
    drop(ahead);
    (stamp(path) == Some(entry.stamp)).then_some(entry)
}
pub struct Worker {
    queue: Arc<(Mutex<VecDeque<Job>>, Condvar)>,
    pub wake: UnixStream,
    results: Receiver<Done>,
}
impl Worker {
    pub fn new(swap: bool) -> std::io::Result<Self> {
        let (wake, mut notify) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
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
        std::thread::Builder::new()
            .name("perch-ahead".into())
            .spawn(move || {
                let (lock, ready) = &*AHEAD;
                let mut ahead = lock.lock().unwrap();
                loop {
                    let Some((&id, plan)) = (ahead.plans.iter_mut()).find(|(_, p)| {
                        p.next < p.paths.len() && p.token.load(Ordering::Relaxed) == p.generation
                    }) else {
                        ahead = ready.wait(ahead).unwrap();
                        continue;
                    };
                    plan.next += 1;
                    let path = plan.paths[plan.next - 1].clone();
                    let (token, generation, (bounds, scale)) =
                        (plan.token.clone(), plan.generation, plan.screen);
                    let fit =
                        |source: &Source| crate::display::fit_view(source.size, bounds, scale);
                    // A source cached without a frame had no room to render one.
                    if (ahead.cache.iter()).any(|e| {
                        e.source.path() == Some(&path)
                            && (e.frame.as_ref()).is_none_or(|f| f.0 == fit(&e.source))
                    }) {
                        continue;
                    }
                    let used = ahead.cache.iter().map(Entry::bytes).sum::<usize>();
                    ahead.busy = Some(path.clone());
                    drop(ahead);
                    let live = || token.load(Ordering::Relaxed) == generation;
                    let entry = (|| -> crate::Result<Entry> {
                        // Stamping first makes a replacement during decoding look stale.
                        let stamp = stamp(&path).ok_or("cannot read image metadata")?;
                        let input = Input::Path(path.clone());
                        let limit = CACHE_BYTES.saturating_sub(used);
                        let source = Source::load(input, swap, limit, &|| !live())?;
                        let view = fit(&source);
                        let frame = (source.bytes() + source.fit_bytes(view) <= limit)
                            .then(|| source.render(view, None, swap, &|| !live()))
                            .transpose()?
                            .map(|frame| (view, frame));
                        Ok(Entry {
                            id,
                            source,
                            stamp,
                            frame,
                        })
                    })();
                    ahead = lock.lock().unwrap();
                    ahead.busy = None;
                    if let Ok(entry) = entry
                        && live()
                        && (ahead.plans.get(&id)).is_some_and(|p| p.paths.contains(&path))
                    {
                        ahead.cache.retain(|e| e.source.path() != Some(&path));
                        let used = ahead.cache.iter().map(Entry::bytes).sum::<usize>();
                        if used + entry.bytes() <= CACHE_BYTES {
                            ahead.cache.push(entry);
                        }
                    }
                    ready.notify_all();
                }
            })?;
        Ok(Self {
            queue,
            wake,
            results,
        })
    }
    pub fn submit(&self, job: Job) -> bool {
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
    pub fn ahead(&self, id: u64, token: &Token, paths: Vec<PathBuf>, screen: ((i32, i32), i32)) {
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
            paths,
            next: 0,
            screen,
        };
        ahead.plans.insert(id, plan);
        ready.notify_all();
    }
    pub fn drain(&self) -> Vec<Done> {
        let mut bytes = [0; 64];
        while (&self.wake).read(&mut bytes).is_ok_and(|n| n != 0) {}
        self.results.try_iter().collect()
    }
    pub fn forget(&self, id: u64) {
        let mut ahead = AHEAD.0.lock().unwrap();
        ahead.plans.remove(&id);
        ahead.cache.retain(|e| e.id != id);
        drop(ahead);
        let _ = self.submit(Job {
            id,
            generation: 0,
            token: token(),
            action: Action::Forget,
        });
    }
}
