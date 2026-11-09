//! Shared prompt transport and draft outcomes for native setup.
//!
//! Standard input/output workers are explicitly owned and joined. Unix workers
//! use nonblocking descriptors and bounded polling; Windows pending synchronous
//! I/O is cancelled before joining. No runtime blocking-pool stdin task survives
//! cancellation or EOF.

pub mod cli;
pub mod state;
pub mod tui;
pub mod widgets;

use std::io;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll};
use std::thread::JoinHandle;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::config::{ConfigError, SaveOutcome};

#[derive(Debug)]
pub enum SetupOutcome {
    Saved(SaveOutcome),
    NotSaved,
}

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("setup interrupted; no implicit save was performed")]
    Interrupted,
    #[error("prompt I/O failed ({0:?})")]
    Io(io::ErrorKind),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Api(#[from] crate::api::ApiError),
}

impl From<io::Error> for SetupError {
    fn from(error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::Interrupted {
            Self::Interrupted
        } else {
            Self::Io(error.kind())
        }
    }
}

/// Injectable, cancellation-safe line input and flushed prompt output.
/// `new` accepts a scripted Cursor/duplex stream or another async transport;
/// `stdio` owns the production operating-system input/output lifecycle.
pub struct PromptIo {
    reader: Box<dyn AsyncBufRead + Send + Unpin>,
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    pending_line: Vec<u8>,
    // Declared after transports: restore shared descriptor flags only after
    // both workers have stopped, even when stdin/stdout share a tty handle.
    #[cfg(unix)]
    _standard_flags: Option<StandardFlags>,
}

impl PromptIo {
    pub fn new(
        reader: impl AsyncBufRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        Self {
            reader: Box::new(reader),
            writer: Box::new(writer),
            pending_line: Vec::new(),
            #[cfg(unix)]
            _standard_flags: None,
        }
    }

    pub fn scripted(input: impl Into<Vec<u8>>) -> Self {
        Self::new(std::io::Cursor::new(input.into()), tokio::io::sink())
    }

    pub fn stdio() -> Result<Self, SetupError> {
        #[cfg(unix)]
        let flags = StandardFlags::capture()?;
        let prompt = Self::new(BufReader::new(NativeInput::new()?), NativeOutput::new()?);
        #[cfg(unix)]
        let prompt = Self {
            _standard_flags: Some(flags),
            ..prompt
        };
        Ok(prompt)
    }

    pub async fn write(
        &mut self,
        text: &str,
        shutdown: &CancellationToken,
    ) -> Result<(), SetupError> {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => Err(SetupError::Interrupted),
            result = async {
                self.writer.write_all(text.as_bytes()).await?;
                self.writer.flush().await
            } => result.map_err(SetupError::from),
        }
    }

    pub async fn read_line(
        &mut self,
        text: &str,
        shutdown: &CancellationToken,
    ) -> Result<String, SetupError> {
        self.write(text, shutdown).await?;
        let count = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Err(SetupError::Interrupted),
            result = self.reader.read_until(b'\n', &mut self.pending_line) => result?,
        };
        if count == 0 {
            return Err(SetupError::Interrupted);
        }
        let mut bytes = std::mem::take(&mut self.pending_line);
        if bytes.contains(&3) {
            return Err(SetupError::Interrupted);
        }
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        String::from_utf8(bytes).map_err(|_| SetupError::Io(io::ErrorKind::InvalidData))
    }
}

struct Worker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    fn spawn(f: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let handle = std::thread::Builder::new()
            .name("setup-stdio".into())
            .spawn(move || f(worker_stop))?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            #[cfg(windows)]
            {
                use std::os::windows::io::AsRawHandle;
                // Cancellation can race the beginning of ReadFile/WriteFile;
                // repeat until the owned worker has observed stop and exited.
                while !handle.is_finished() {
                    unsafe {
                        windows_sys::Win32::System::IO::CancelSynchronousIo(handle.as_raw_handle());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
            let _ = handle.join();
        }
    }
}

struct NativeInput {
    receiver: mpsc::Receiver<io::Result<Vec<u8>>>,
    current: Vec<u8>,
    offset: usize,
    worker: Worker,
}

impl NativeInput {
    fn new() -> io::Result<Self> {
        let mut native = NativeDescriptor::new(true)?;
        let (sender, receiver) = mpsc::channel(8);
        let worker = Worker::spawn(move |stop| {
            while !stop.load(Ordering::Acquire) {
                let mut bytes = vec![0; 4096];
                let result = native.read(&mut bytes, &stop);
                match result {
                    Ok(0) => break,
                    Ok(count) => {
                        bytes.truncate(count);
                        if sender.blocking_send(Ok(bytes)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.blocking_send(Err(error));
                        break;
                    }
                }
            }
        })?;
        Ok(Self {
            receiver,
            current: Vec::new(),
            offset: 0,
            worker,
        })
    }
}

impl AsyncRead for NativeInput {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if this.offset == this.current.len() {
            match this.receiver.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Err(error)),
                Poll::Ready(Some(Ok(bytes))) => {
                    this.current = bytes;
                    this.offset = 0;
                }
            }
        }
        let count = output.remaining().min(this.current.len() - this.offset);
        output.put_slice(&this.current[this.offset..this.offset + count]);
        this.offset += count;
        Poll::Ready(Ok(()))
    }
}

impl Drop for NativeInput {
    fn drop(&mut self) {
        self.receiver.close();
        self.worker.stop();
    }
}

struct OutputRequest {
    bytes: Vec<u8>,
    complete: oneshot::Sender<io::Result<usize>>,
}

struct NativeOutput {
    sender: Option<mpsc::UnboundedSender<OutputRequest>>,
    pending: Option<oneshot::Receiver<io::Result<usize>>>,
    worker: Worker,
}

impl NativeOutput {
    fn new() -> io::Result<Self> {
        let native = NativeDescriptor::new(false)?;
        let (sender, mut receiver) = mpsc::unbounded_channel::<OutputRequest>();
        let worker = Worker::spawn(move |stop| {
            while let Some(request) = receiver.blocking_recv() {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let mut offset = 0;
                let result = loop {
                    if offset == request.bytes.len() {
                        break Ok(offset);
                    }
                    match native.write(&request.bytes[offset..], &stop) {
                        Ok(0) => break Err(io::ErrorKind::WriteZero.into()),
                        Ok(count) => offset += count,
                        Err(error) => break Err(error),
                    }
                };
                let _ = request.complete.send(result);
            }
        })?;
        Ok(Self {
            sender: Some(sender),
            pending: None,
            worker,
        })
    }
}

impl AsyncWrite for NativeOutput {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.as_mut().poll_flush(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => {}
        }
        let this = self.get_mut();
        let (complete, receiver) = oneshot::channel();
        let Some(sender) = &this.sender else {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        };
        // Accepting into this single-request buffer is immediate, so a pending
        // write never secretly consumes bytes. Flush observes the OS result.
        if sender
            .send(OutputRequest {
                bytes: bytes.to_vec(),
                complete,
            })
            .is_err()
        {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        this.pending = Some(receiver);
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let Some(receiver) = &mut this.pending else {
            return Poll::Ready(Ok(()));
        };
        match Pin::new(receiver).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                this.pending = None;
                Poll::Ready(
                    result
                        .unwrap_or_else(|_| Err(io::ErrorKind::BrokenPipe.into()))
                        .map(|_| ()),
                )
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl Drop for NativeOutput {
    fn drop(&mut self) {
        self.sender.take();
        self.worker.stop();
    }
}

#[cfg(unix)]
struct NativeDescriptor {
    fd: std::os::fd::RawFd,
}

#[cfg(unix)]
impl NativeDescriptor {
    fn new(input: bool) -> io::Result<Self> {
        let fd = unsafe {
            libc::dup(if input {
                libc::STDIN_FILENO
            } else {
                libc::STDOUT_FILENO
            })
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            return Err(error);
        }
        Ok(Self { fd })
    }

    fn ready(&self, events: libc::c_short, stop: &AtomicBool) -> io::Result<()> {
        while !stop.load(Ordering::Acquire) {
            let mut descriptor = libc::pollfd {
                fd: self.fd,
                events,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut descriptor, 1, 25) };
            if result > 0 {
                return Ok(());
            }
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
        Err(io::ErrorKind::Interrupted.into())
    }

    fn read(&mut self, bytes: &mut [u8], stop: &AtomicBool) -> io::Result<usize> {
        loop {
            self.ready(libc::POLLIN, stop)?;
            let count = unsafe { libc::read(self.fd, bytes.as_mut_ptr().cast(), bytes.len()) };
            if count >= 0 {
                return Ok(count as usize);
            }
            let error = io::Error::last_os_error();
            if !matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                return Err(error);
            }
        }
    }

    fn write(&self, bytes: &[u8], stop: &AtomicBool) -> io::Result<usize> {
        loop {
            self.ready(libc::POLLOUT, stop)?;
            let count = unsafe { libc::write(self.fd, bytes.as_ptr().cast(), bytes.len()) };
            if count >= 0 {
                return Ok(count as usize);
            }
            let error = io::Error::last_os_error();
            if !matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                return Err(error);
            }
        }
    }
}

#[cfg(unix)]
impl Drop for NativeDescriptor {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

#[cfg(unix)]
struct StandardFlags([(libc::c_int, libc::c_int); 2]);

#[cfg(unix)]
impl StandardFlags {
    fn capture() -> io::Result<Self> {
        let input = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) };
        let output = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_GETFL) };
        if input < 0 || output < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self([
            (libc::STDIN_FILENO, input),
            (libc::STDOUT_FILENO, output),
        ]))
    }
}

#[cfg(unix)]
impl Drop for StandardFlags {
    fn drop(&mut self) {
        for (fd, flags) in self.0 {
            unsafe {
                libc::fcntl(fd, libc::F_SETFL, flags);
            }
        }
    }
}

#[cfg(windows)]
struct NativeDescriptor {
    input: bool,
    trailing_high_surrogate: Option<u16>,
}

#[cfg(windows)]
impl NativeDescriptor {
    fn new(input: bool) -> io::Result<Self> {
        Ok(Self {
            input,
            trailing_high_surrogate: None,
        })
    }

    fn handle(&self) -> io::Result<windows_sys::Win32::Foundation::HANDLE> {
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        let handle = unsafe {
            GetStdHandle(if self.input {
                STD_INPUT_HANDLE
            } else {
                STD_OUTPUT_HANDLE
            })
        };
        if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(handle)
        }
    }

    fn read(&mut self, bytes: &mut [u8], stop: &AtomicBool) -> io::Result<usize> {
        let handle = self.handle()?;
        let mut mode = 0;
        if unsafe { windows_sys::Win32::System::Console::GetConsoleMode(handle, &mut mode) } != 0 {
            loop {
                if stop.load(Ordering::Acquire) {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                // Console APIs operate in UTF-16 regardless of the active
                // console code page. A split surrogate survives the next read.
                let mut units = [0u16; 1024];
                let prefix = if let Some(unit) = self.trailing_high_surrogate.take() {
                    units[0] = unit;
                    1
                } else {
                    0
                };
                let mut count = 0;
                let result = unsafe {
                    windows_sys::Win32::System::Console::ReadConsoleW(
                        handle,
                        units[prefix..].as_mut_ptr().cast(),
                        (units.len() - prefix) as u32,
                        &mut count,
                        std::ptr::null(),
                    )
                };
                if result == 0 {
                    return Err(io::Error::last_os_error());
                }
                if count == 0 {
                    return if prefix == 0 {
                        Ok(0)
                    } else {
                        Err(io::ErrorKind::InvalidData.into())
                    };
                }
                // Ctrl+Z at the start of a Windows console line is EOF.
                if prefix == 0 && units[0] == 26 {
                    return Ok(0);
                }
                let mut end = prefix + count as usize;
                if (0xD800..=0xDBFF).contains(&units[end - 1]) {
                    end -= 1;
                    self.trailing_high_surrogate = Some(units[end]);
                }
                if end == 0 {
                    continue;
                }
                let text = String::from_utf16(&units[..end])
                    .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
                bytes[..text.len()].copy_from_slice(text.as_bytes());
                return Ok(text.len());
            }
        }
        if stop.load(Ordering::Acquire) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let mut count = 0;
        let result = unsafe {
            windows_sys::Win32::Storage::FileSystem::ReadFile(
                handle,
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                &mut count,
                std::ptr::null_mut(),
            )
        };
        if result != 0 {
            Ok(count as usize)
        } else {
            let error = io::Error::last_os_error();
            if error.raw_os_error()
                == Some(windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE as i32)
            {
                Ok(0)
            } else {
                Err(error)
            }
        }
    }

    fn write(&self, bytes: &[u8], stop: &AtomicBool) -> io::Result<usize> {
        if stop.load(Ordering::Acquire) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let handle = self.handle()?;
        let mut mode = 0;
        if unsafe { windows_sys::Win32::System::Console::GetConsoleMode(handle, &mut mode) } != 0 {
            let text = std::str::from_utf8(bytes)
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
            let units: Vec<u16> = text.encode_utf16().collect();
            let mut offset = 0;
            while offset < units.len() {
                if stop.load(Ordering::Acquire) {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let mut count = 0;
                let result = unsafe {
                    windows_sys::Win32::System::Console::WriteConsoleW(
                        handle,
                        units[offset..].as_ptr().cast(),
                        (units.len() - offset).min(u32::MAX as usize) as u32,
                        &mut count,
                        std::ptr::null(),
                    )
                };
                if result == 0 {
                    return Err(io::Error::last_os_error());
                }
                if count == 0 {
                    return Err(io::ErrorKind::WriteZero.into());
                }
                offset += count as usize;
            }
            return Ok(bytes.len());
        }
        let mut count = 0;
        let result = unsafe {
            windows_sys::Win32::Storage::FileSystem::WriteFile(
                handle,
                bytes.as_ptr(),
                bytes.len().min(u32::MAX as usize) as u32,
                &mut count,
                std::ptr::null_mut(),
            )
        };
        if result != 0 {
            Ok(count as usize)
        } else {
            Err(io::Error::last_os_error())
        }
    }
}
