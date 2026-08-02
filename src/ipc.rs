use crate::{Result, document::Input};
use rustix::{
    fs::{
        self, MemfdFlags, Mode, OFlags, SealFlags, fcntl_add_seals, fcntl_get_seals, memfd_create,
    },
    io::Errno,
    net::{
        self, AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
        SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketAddrUnix, SocketFlags,
        SocketType,
    },
    process::getuid,
};
use std::{
    env,
    ffi::OsString,
    fs::File,
    io::{self, BufRead, BufReader, BufWriter, IoSlice, IoSliceMut, Read, Write},
    mem::MaybeUninit,
    os::{
        fd::{AsFd, AsRawFd, OwnedFd},
        unix::ffi::{OsStrExt, OsStringExt},
    },
    path::PathBuf,
    sync::Arc,
};

const MAX_PATHS: usize = 4096;
const MAX_PATH: usize = 4096;
const IMMUTABLE: SealFlags = SealFlags::GROW
    .union(SealFlags::SHRINK)
    .union(SealFlags::WRITE);
pub const MAX_CLIENTS: usize = 16;

fn address() -> Result<SocketAddrUnix> {
    let name = env::var_os("PERCH_SOCKET")
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("perch-{}", getuid().as_raw()).into());
    Ok(SocketAddrUnix::new_abstract_name(name.as_bytes())?)
}

fn socket(flags: SocketFlags) -> rustix::io::Result<OwnedFd> {
    net::socket_with(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        flags | SocketFlags::CLOEXEC,
        None,
    )
}

pub fn server() -> Result<OwnedFd> {
    let fd = socket(SocketFlags::NONBLOCK)?;
    net::bind(&fd, &address()?)?;
    net::listen(&fd, MAX_CLIENTS as i32)?;
    Ok(fd)
}

pub fn accept(listener: &OwnedFd) -> rustix::io::Result<OwnedFd> {
    let fd = net::accept_with(listener, SocketFlags::CLOEXEC | SocketFlags::NONBLOCK)?;
    if net::sockopt::socket_peercred(&fd)?.uid != getuid() {
        return Err(Errno::ACCESS);
    }
    Ok(fd)
}

fn retry<T>(mut operation: impl FnMut() -> rustix::io::Result<T>) -> rustix::io::Result<T> {
    loop {
        match operation() {
            Err(Errno::INTR) => continue,
            result => return result,
        }
    }
}

pub fn reply(fd: &OwnedFd, success: bool) -> rustix::io::Result<()> {
    let bytes = [u8::from(!success)];
    if retry(|| net::send(fd, &bytes, SendFlags::NOSIGNAL))? == bytes.len() {
        Ok(())
    } else {
        Err(Errno::IO)
    }
}

pub enum Request {
    Paths(File),
    Stdin(Arc<File>),
}

impl Request {
    pub fn inputs(&self) -> Result<Vec<Input>> {
        let file = match self {
            Self::Paths(file) => file,
            Self::Stdin(file) if file.metadata()?.is_file() => {
                return Ok(vec![Input::File(file.clone())]);
            }
            Self::Stdin(_) => return Err("invalid stdin storage".into()),
        };
        if !fcntl_get_seals(file)?.contains(IMMUTABLE) {
            return Err("path list is not sealed".into());
        }
        let mut reader = BufReader::new(File::open(format!("/proc/self/fd/{}", file.as_raw_fd()))?);
        let mut inputs = Vec::new();
        loop {
            let mut bytes = Vec::new();
            if (&mut reader)
                .take((MAX_PATH + 1) as u64)
                .read_until(0, &mut bytes)?
                == 0
            {
                break;
            }
            if bytes.len() == 1 || bytes.pop() != Some(0) || inputs.len() == MAX_PATHS {
                return Err("invalid image path list".into());
            }
            inputs.push(Input::Path(OsString::from_vec(bytes).into()));
        }
        if inputs.is_empty() {
            return Err("empty image path list".into());
        }
        Ok(inputs)
    }
}

pub fn receive(fd: &OwnedFd) -> Result<Option<Request>> {
    let mut buffer = [0; 5];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let message = match retry(|| {
        net::recvmsg(
            fd,
            &mut [IoSliceMut::new(&mut buffer)],
            &mut control,
            RecvFlags::CMSG_CLOEXEC,
        )
    }) {
        Err(Errno::AGAIN) => return Ok(None),
        result => result?,
    };
    if message.bytes != buffer.len()
        || message
            .flags
            .intersects(ReturnFlags::TRUNC | ReturnFlags::CTRUNC)
    {
        return Err("invalid request packet".into());
    }
    let mut descriptors = control
        .drain()
        .flat_map(|message| match message {
            RecvAncillaryMessage::ScmRights(fds) => Some(fds),
            _ => None,
        })
        .flatten();
    let file = File::from(descriptors.next().ok_or("missing request descriptor")?);
    if descriptors.next().is_some() {
        return Err("invalid request descriptor".into());
    }
    match &buffer {
        b"PER2P" => Ok(Some(Request::Paths(file))),
        b"PER2S" => Ok(Some(Request::Stdin(Arc::new(file)))),
        _ => Err("invalid request header".into()),
    }
}

pub fn run_client(args: &[OsString]) -> Result<()> {
    let stdin = args.len() == 1 && (args[0] == "--stdin" || args[0] == "-");
    if args.is_empty() || args.len() > MAX_PATHS {
        return Err("usage: perch [--stdin | IMAGE...]".into());
    }
    let fd = socket(SocketFlags::empty())?;
    net::connect(&fd, &address()?).map_err(|error| format!("daemon is unavailable: {error}"))?;
    let (header, file) = if stdin {
        let mut file = File::from(fs::open(
            env::temp_dir(),
            OFlags::TMPFILE | OFlags::RDWR | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        if io::copy(&mut io::stdin().lock(), &mut file)? == 0 {
            return Err("cannot open stdin: empty stream".into());
        }
        (b"PER2S", file)
    } else {
        let fd = memfd_create(
            c"perch-paths",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )?;
        let mut writer = BufWriter::new(File::from(fd));
        for arg in args {
            let path = std::fs::canonicalize(arg).map_err(|error| {
                format!("cannot resolve {}: {error}", PathBuf::from(arg).display())
            })?;
            let bytes = path.as_os_str().as_bytes();
            if bytes.len() > MAX_PATH {
                return Err("image path is too long".into());
            }
            writer.write_all(bytes)?;
            writer.write_all(&[0])?;
        }
        let file = writer.into_inner()?;
        fcntl_add_seals(&file, IMMUTABLE)?;
        (b"PER2P", file)
    };
    let fds = [file.as_fd()];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    assert!(control.push(SendAncillaryMessage::ScmRights(&fds)));
    if retry(|| {
        net::sendmsg(
            &fd,
            &[IoSlice::new(header)],
            &mut control,
            SendFlags::NOSIGNAL,
        )
    })? != header.len()
    {
        return Err("cannot send request".into());
    }
    let mut response = [1];
    let (_, size) = retry(|| net::recv(&fd, &mut response[..], RecvFlags::TRUNC))?;
    if size == 1 && response[0] == 0 {
        Ok(())
    } else {
        Err("daemon could not open images".into())
    }
}
