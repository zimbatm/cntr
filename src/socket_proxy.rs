use nix::fcntl::{fcntl, OFlag, FcntlArg};
use nix::sys::epoll::{epoll_create1, epoll_ctl, epoll_wait, EpollCreateFlags, EpollOp, EpollEvent,
                      EpollFlags};
use nix::sys::socket::{accept4, SockFlag};
use nix::unistd::pipe2;
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::io::prelude::*;
use std::os::unix::net::UnixListener;
use std::os::unix::prelude::*;
use types::{Result, Error};

struct Pipe {
    reader: File,
    writer: File,
    size: usize,
}

struct FilePair {
    server_fd: File,
    client_fd: File,
    server_to_client_pipe: Pipe,
    client_to_server_pipe: Pipe,
    server_to_client_buffer_full: usize,
    client_to_server_buffer_full: usize,
}

// take from systemd's socket-proxyd
const BUFFER_SIZE: usize = 256 * 1024;

impl Pipe {
    fn new() -> Result<Pipe> {
        let (read_fd, write_fd) = tryfmt!(
            pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK),
            "failed to create pipe"
        );

        let (reader, writer) = unsafe { (File::from_raw_fd(read_fd), File::from_raw_fd(write_fd)) };

        let _ = fcntl(
            reader.as_raw_fd(),
            FcntlArg::F_SETPIPE_SZ(BUFFER_SIZE as i32),
        );

        let size = tryfmt!(
            fcntl(reader.as_raw_fd(), FcntlArg::F_GETPIPE_SZ),
            "failed to get pipe size"
        );

        Ok(Pipe {
            reader: reader,
            writer: writer,
            size: size as usize,
        })
    }
}

impl FilePair {
    pub fn new(server_fd: File, client_fd: File) -> Result<FilePair> {
        Ok(FilePair {
            server_fd: server_fd,
            client_fd: client_fd,
            server_to_client_pipe: tryfmt!(Pipe::new(), "failed to create parent pipe"),
            client_to_server_pipe: tryfmt!(Pipe::new(), "failed to create child pipe"),
            server_to_client_buffer_full: 0,
            client_to_server_buffer_full: 0,
        })
    }
}

trait Callback {
    fn process(&self, context: &mut Context, file: &File, flags: EpollFlags) -> Result<()>;
}

struct Context<'a> {
    epoll_file: File,
    callbacks: HashMap<RawFd, &'a Callback>,
}

impl<'a> Context<'a> {
    pub fn new() -> Result<Context<'a>> {
        let fd = tryfmt!(
            epoll_create1(EpollCreateFlags::EPOLL_CLOEXEC),
            "failed to create epoll socket"
        );
        Ok(Context {
            epoll_file: unsafe { File::from_raw_fd(fd) },
            callbacks: HashMap::new(),
        })
    }

    pub fn add_file(&mut self, file: File, callback: &'a Callback) -> Result<i32> {
        let mut event = EpollEvent::new(EpollFlags::EPOLLIN | EpollFlags::EPOLLERR, 1);
        let res = epoll_ctl(
            self.epoll_file.as_raw_fd(),
            EpollOp::EpollCtlAdd,
            file.as_raw_fd(),
            &mut event,
        );
        tryfmt!(res, "failed to add file descriptor to epoll socket");

        let id = file.as_raw_fd();
        let old_file = self.callbacks.insert(file.into_raw_fd(), callback);
        assert!(old_file.is_none());
        Ok(id)
    }

    pub fn remove_file(&mut self, id: i32) -> Result<()> {
        self.callbacks.remove(&id);
        let file = unsafe { File::from_raw_fd(id) };
        let res = tryfmt!(
            epoll_ctl(
                self.epoll_file.as_raw_fd(),
                EpollOp::EpollCtlDel,
                file.as_raw_fd(),
                None,
            ),
            "failed to remove file"
        );
        Ok(())
    }

    pub fn select<'b>(&self, events: &'b mut [EpollEvent]) -> Result<&'b [EpollEvent]> {
        let res = epoll_wait(self.epoll_file.as_raw_fd(), events, 0);
        let count = tryfmt!(res, "failed to wait for epoll events");
        Ok(&events[..count])
    }

    pub fn process(&mut self, event: &EpollEvent) -> Result<()> {
        let file = unsafe { File::from_raw_fd(event.data() as RawFd) };
        let callback = self.callbacks[&file.as_raw_fd()];
        let res = callback.process(self, &file, event.events());
        file.into_raw_fd();
        res
    }
}

struct AcceptCb {}
impl Callback for AcceptCb {
    fn process(&self, context: &mut Context, file: &File, flags: EpollFlags) -> Result<()> {
        assert!(flags == EpollFlags::EPOLLIN);
        let fd = tryfmt!(
            accept4(
                file.as_raw_fd(),
                SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
            ),
            "failed to accept connections on socket"
        );
        Ok(())

    }
}

struct TrafficCb {}
impl Callback for TrafficCb {
    fn process(&self, context: &mut Context, file: &File, flags: EpollFlags) -> Result<()> {
        Ok(())
    }
}

pub struct Awakener {
    pipe: Pipe,
}

impl Awakener {
    pub fn new() -> Result<Awakener> {
        let (read_fd, write_fd) = tryfmt!(
            pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK),
            "failed to create pipe"
        );
        let pipe = unsafe {
            Pipe {
                reader: File::from_raw_fd(read_fd),
                writer: File::from_raw_fd(write_fd),
                size: 0,
            }
        };
        Ok(Awakener { pipe: pipe })
    }

    pub fn wakeup(&self) -> io::Result<()> {
        match (&self.pipe.writer).write(&[1]) {
            Ok(_) => Ok(()),
            Err(e) => {
                if e.kind() == io::ErrorKind::WouldBlock {
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }
}

pub fn forward(sockets: Vec<UnixListener>, awakener: Awakener) -> Result<()> {
    let cb = AcceptCb {};
    let mut context = tryfmt!(Context::new(), "failed to create epoll context");
    let mut listening_fds = Vec::new();
    listening_fds.reserve(sockets.len());

    for socket in sockets {
        tryfmt!(
            socket.set_nonblocking(true),
            "failed to set socket nonblocking"
        );
        listening_fds.push(socket.as_raw_fd());
        let file = unsafe { File::from_raw_fd(socket.into_raw_fd()) };
        tryfmt!(
            context.add_file(file, &cb),
            "could not add unix socket to event loop"
        );
    }

    let mut events = Vec::new();
    events.reserve(1024);

    loop {
        let selected_events = tryfmt!(
            context.select(&mut events),
            "failed to wait for listening sockets"
        );
        for event in selected_events {
            if event.data() == awakener.pipe.reader.as_raw_fd() as u64 {
                return Ok(());
            }
            tryfmt!(context.process(event), "failed to process epoll event");
        }
    }
}
