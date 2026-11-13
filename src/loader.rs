use crate::document::{self, Animation, CACHE_BYTES, Input, Source, View};
use crate::image::Frame;
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver},
    },
    time::Duration,
};

pub type Token = Arc<AtomicU64>;
pub fn token() -> Token {
    Arc::new(AtomicU64::new(0))
}
pub fn cancel(token: &Token) -> u64 {
    token.fetch_add(1, Ordering::Relaxed) + 1
}
#[derive(Clone, Copy)]
pub enum Step {
    Still,
    Next,
    Restart,
}
pub enum Action {
    Open(Arc<Vec<Input>>),
    Load(Input),
    Render {
        source: Arc<Source>,
        view: View,
        step: Step,
    },
    Preload(Input),
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
            Action::Preload(_) => 3,
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
    Loaded(Arc<Source>),
    Rendered {
        frame: Arc<Frame>,
        delay: Option<Duration>,
        stepped: bool,
        ended: bool,
    },
}
pub struct Done {
    pub id: u64,
    pub generation: u64,
    pub result: std::result::Result<Output, String>,
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
                let mut cache: Option<(u64, PathBuf, Arc<Source>)> = None;
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
                    // Speculative jobs have no result, but freeing queue space must wake retries.
                    if was_full && notify.write_all(&[1]).is_err() {
                        break;
                    }
                    if !job.live() {
                        continue;
                    }
                    if matches!(job.action, Action::Forget) {
                        animations.remove(&job.id);
                        if cache.as_ref().is_some_and(|c| c.0 == job.id) {
                            cache = None;
                        }
                        continue;
                    }
                    let load = |input: Input, cache: &mut Option<(u64, PathBuf, Arc<Source>)>| {
                        if cache
                            .as_ref()
                            .is_some_and(|c| input.path() == Some(c.1.as_path()))
                        {
                            Ok(cache.take().unwrap().2)
                        } else {
                            Source::load(input, swap, usize::MAX)
                        }
                    };
                    let result = (|| -> crate::Result<Option<Output>> {
                        match &job.action {
                            Action::Open(inputs) => {
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
                                    if !job.live() {
                                        return Ok(None);
                                    }
                                    sources.push(load(input.clone(), &mut cache)?);
                                }
                                Ok(Some(Output::Opened {
                                    sources,
                                    paths: Arc::new(paths),
                                }))
                            }
                            Action::Load(input) => {
                                animations.remove(&job.id);
                                Ok(Some(Output::Loaded(load(input.clone(), &mut cache)?)))
                            }
                            Action::Preload(input) => {
                                if cache
                                    .as_ref()
                                    .is_some_and(|c| input.path() == Some(c.1.as_path()))
                                {
                                    return Ok(None);
                                }
                                let source = Source::load(input.clone(), swap, CACHE_BYTES)?;
                                if job.live()
                                    && source.bytes() <= CACHE_BYTES
                                    && let Some(path) = source.path()
                                {
                                    cache = Some((job.id, path.to_path_buf(), source));
                                }
                                Ok(None)
                            }
                            Action::Render { source, view, step } => {
                                let stepped = !matches!(step, Step::Still);
                                let mut delay = None;
                                let mut ended = false;
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
                                    ended = delay.is_none();
                                }
                                let current = animations
                                    .get(&job.id)
                                    .filter(|a| Arc::ptr_eq(&a.source, source))
                                    .and_then(|a| a.current.as_ref());
                                let frame = source.render(*view, current, swap)?;
                                Ok(Some(Output::Rendered {
                                    frame,
                                    delay,
                                    stepped,
                                    ended,
                                }))
                            }
                            Action::Forget => unreachable!(),
                        }
                    })();
                    if !job.live() {
                        continue;
                    }
                    let result = match result {
                        Ok(Some(output)) => Ok(output),
                        Ok(None) => continue,
                        Err(error) => {
                            if matches!(job.action, Action::Preload(_)) {
                                continue;
                            }
                            Err(error.to_string())
                        }
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
        Ok(Self {
            queue,
            wake,
            results,
        })
    }
    pub fn submit(&self, job: Job) -> bool {
        let (lock, ready) = &*self.queue;
        let mut queue = lock.lock().unwrap();
        queue.retain(|old| old.id != job.id);
        queue.retain(Job::live);
        if queue.len() >= 32 && !matches!(job.action, Action::Forget) {
            return false;
        }
        queue.push_back(job);
        ready.notify_one();
        true
    }
    pub fn drain(&self) -> Vec<Done> {
        let mut bytes = [0; 64];
        while (&self.wake).read(&mut bytes).is_ok_and(|n| n != 0) {}
        self.results.try_iter().collect()
    }
    pub fn forget(&self, id: u64) {
        let _ = self.submit(Job {
            id,
            generation: 0,
            token: token(),
            action: Action::Forget,
        });
    }
}
