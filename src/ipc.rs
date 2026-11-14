use crate::Result;
use nix::{
    errno::Errno,
    sys::socket::{
        self, AddressFamily, ControlMessage, ControlMessageOwned, MsgFlags, SockFlag, SockType,
        UnixAddr,
    },
    unistd::getuid,
};
use std::{
    env,
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{self, IoSlice, IoSliceMut},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::OpenOptionsExt,
        },
    },
    path::PathBuf,
};

const MAGIC: u32 = 0x50455244;
const MAX_PATHS: usize = 4096;
const MAX_PATH_SIZE: usize = 4096;
pub const MAX_CLIENTS: usize = 16;

fn address() -> Result<UnixAddr> {
    let name = env::var_os("PERCH_SOCKET")
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("perch-{}", getuid()).into());
    Ok(UnixAddr::new_abstract(name.as_bytes()).map_err(|_| "socket name is too long")?)
}

fn new_socket(flags: SockFlag) -> nix::Result<OwnedFd> {
    socket::socket(
        AddressFamily::Unix,
        SockType::SeqPacket,
        flags | SockFlag::SOCK_CLOEXEC,
        None,
    )
}

pub fn server() -> Result<OwnedFd> {
    let fd = new_socket(SockFlag::SOCK_NONBLOCK)?;
    socket::bind(fd.as_raw_fd(), &address()?)?;
    socket::listen(&fd, socket::Backlog::new(MAX_CLIENTS as i32)?)?;
    Ok(fd)
}

pub fn accept(server: &OwnedFd) -> nix::Result<OwnedFd> {
    let fd = socket::accept4(
        server.as_raw_fd(),
        SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
    )?;
    // accept4 returns a new descriptor owned by this client.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if socket::getsockopt(&fd, socket::sockopt::PeerCredentials)?.uid() != getuid().as_raw() {
        return Err(Errno::EACCES);
    }
    Ok(fd)
}

pub fn send(fd: &OwnedFd, bytes: &[u8]) -> nix::Result<()> {
    loop {
        match socket::send(fd.as_raw_fd(), bytes, MsgFlags::MSG_NOSIGNAL) {
            Err(Errno::EINTR) => continue,
            Ok(size) if size == bytes.len() => return Ok(()),
            Ok(_) => return Err(Errno::EIO),
            Err(error) => return Err(error),
        }
    }
}

fn receive(fd: &OwnedFd, bytes: &mut [u8]) -> nix::Result<usize> {
    loop {
        match socket::recv(fd.as_raw_fd(), bytes, MsgFlags::MSG_TRUNC) {
            Err(Errno::EINTR) => continue,
            result => return result,
        }
    }
}

fn send_file(fd: &OwnedFd, file: &File) -> nix::Result<()> {
    let rights = [file.as_raw_fd()];
    loop {
        match socket::sendmsg::<()>(
            fd.as_raw_fd(),
            &[IoSlice::new(&[0])],
            &[ControlMessage::ScmRights(&rights)],
            MsgFlags::MSG_NOSIGNAL,
            None,
        ) {
            Err(Errno::EINTR) => continue,
            Ok(1) => return Ok(()),
            Ok(_) => return Err(Errno::EIO),
            Err(error) => return Err(error),
        }
    }
}
fn receive_file(fd: &OwnedFd) -> Result<File> {
    let mut marker = [1];
    let mut iov = [IoSliceMut::new(&mut marker)];
    let mut space = nix::cmsg_space!([i32; 1]);
    let message = loop {
        match socket::recvmsg::<()>(
            fd.as_raw_fd(),
            &mut iov,
            Some(&mut space),
            MsgFlags::MSG_CMSG_CLOEXEC,
        ) {
            Err(Errno::EINTR) => continue,
            result => break result?,
        }
    };
    let mut files: Vec<File> = message
        .cmsgs()?
        .flat_map(|control| match control {
            ControlMessageOwned::ScmRights(fds) => fds,
            _ => Vec::new(),
        })
        .map(|raw| {
            // SCM_RIGHTS transfers ownership of each newly received descriptor.
            unsafe { File::from_raw_fd(raw) }
        })
        .collect();
    if message.bytes != 1 || files.len() != 1 || marker[0] != 0 {
        return Err("daemon did not provide stdin storage".into());
    }
    Ok(files.remove(0))
}

enum Request {
    Header,
    Paths {
        expected: usize,
        paths: Vec<PathBuf>,
    },
    Stdin(File),
}

pub enum Images {
    Paths(Vec<PathBuf>),
    Stdin(File),
}

pub struct Client {
    pub fd: OwnedFd,
    request: Request,
}

impl Client {
    pub fn new(fd: OwnedFd) -> Self {
        Self {
            fd,
            request: Request::Header,
        }
    }

    pub fn read(&mut self) -> Result<Option<Images>> {
        // Drain a bounded batch so short requests avoid another poll without starving peers.
        for _ in 0..16 {
            match self.read_packet() {
                Err(error) if error.downcast_ref::<Errno>() == Some(&Errno::EAGAIN) => {
                    return Ok(None);
                }
                Ok(None) => {}
                result => return result,
            }
        }
        Ok(None)
    }

    fn read_packet(&mut self) -> Result<Option<Images>> {
        let mut packet = [0; MAX_PATH_SIZE + 1];
        let size = receive(&self.fd, &mut packet)?;
        if size == 0 || size > packet.len() {
            return Err("invalid request packet".into());
        }
        match &mut self.request {
            Request::Header => {
                if size != 8 {
                    return Err("invalid request header".into());
                }
                let magic = u32::from_ne_bytes(packet[..4].try_into()?);
                let count = u32::from_ne_bytes(packet[4..8].try_into()?) as usize;
                if magic != MAGIC || count > MAX_PATHS {
                    return Err("invalid request header".into());
                }
                self.request = if count == 0 {
                    let file = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .mode(0o600)
                        .custom_flags(nix::libc::O_TMPFILE)
                        .open(env::temp_dir())?;
                    send_file(&self.fd, &file)?;
                    Request::Stdin(file)
                } else {
                    Request::Paths {
                        expected: count,
                        paths: Vec::with_capacity(count),
                    }
                };
                Ok(None)
            }
            Request::Paths { expected, paths } => {
                if size <= 1 || packet[size - 1] != 0 || packet[..size - 1].contains(&0) {
                    return Err("invalid image path".into());
                }
                paths.push(OsString::from_vec(packet[..size - 1].to_vec()).into());
                Ok((paths.len() == *expected).then(|| Images::Paths(std::mem::take(paths))))
            }
            Request::Stdin(_) => match (packet[0], size) {
                (0, 1) => {
                    let Request::Stdin(file) =
                        std::mem::replace(&mut self.request, Request::Header)
                    else {
                        unreachable!();
                    };
                    Ok(Some(Images::Stdin(file)))
                }
                _ => Err("invalid stdin packet".into()),
            },
        }
    }
}

pub fn run_client(args: &[OsString]) -> Result<()> {
    let from_stdin = args.len() == 1 && (args[0] == "--stdin" || args[0] == "-");
    if args.is_empty() || args.len() > MAX_PATHS {
        return Err("usage: perch [--stdin | IMAGE...]".into());
    }
    let fd = new_socket(SockFlag::empty())?;
    socket::connect(fd.as_raw_fd(), &address()?)
        .map_err(|error| format!("daemon is unavailable: {error}"))?;
    let mut header = [0; 8];
    header[..4].copy_from_slice(&MAGIC.to_ne_bytes());
    header[4..].copy_from_slice(&(if from_stdin { 0 } else { args.len() as u32 }).to_ne_bytes());
    send(&fd, &header)?;
    if from_stdin {
        let mut file = receive_file(&fd)?;
        if io::copy(&mut io::stdin().lock(), &mut file)? == 0 {
            return Err("cannot open stdin: empty stream".into());
        }
        send(&fd, &[0])?;
    } else {
        for arg in args {
            let path = std::fs::canonicalize(arg).map_err(|error| {
                format!("cannot resolve {}: {error}", PathBuf::from(arg).display())
            })?;
            let mut bytes = path.into_os_string().into_vec();
            if bytes.len() > MAX_PATH_SIZE {
                return Err("image path is too long".into());
            }
            bytes.push(0);
            send(&fd, &bytes)?;
        }
    }
    let mut response = [1];
    if receive(&fd, &mut response)? == 1 && response[0] == 0 {
        Ok(())
    } else {
        Err("daemon could not open images".into())
    }
}
