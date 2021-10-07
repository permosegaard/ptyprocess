use crate::control_code::ControlCode;
use crate::stream::Stream;
#[cfg(feature = "async")]
use futures_lite::AsyncWriteExt;
use nix::errno::{self, Errno};
use nix::fcntl::{fcntl, open, FcntlArg, FdFlag, OFlag};
use nix::libc::{self, winsize, STDERR_FILENO, STDIN_FILENO, STDOUT_FILENO};
use nix::pty::PtyMaster;
use nix::pty::{grantpt, posix_openpt, unlockpt};
use nix::sys::stat::Mode;
use nix::sys::wait::{self, waitpid, WaitStatus};
use nix::sys::{signal, termios};
use nix::unistd::{
    self, close, dup, dup2, fork, isatty, pipe, setsid, sysconf, write, ForkResult, Pid, SysconfVar,
};
use nix::{ioctl_write_ptr_bad, Error, Result};
use signal::Signal::SIGKILL;
use std::convert::TryInto;
use std::fs::File;
use std::io::Write;
use std::ops::{Deref, DerefMut};
use std::os::unix::prelude::{AsRawFd, CommandExt, FromRawFd, RawFd};
use std::process::{self, Command};
use std::time::{self, Duration};
use std::{io, thread};
use termios::SpecialCharacterIndices;

const DEFAULT_TERM_COLS: u16 = 80;
const DEFAULT_TERM_ROWS: u16 = 24;
const DEFAULT_VEOF_CHAR: u8 = 0x4; // ^D
const DEFAULT_INTR_CHAR: u8 = 0x3; // ^C

/// PtyProcess controls a spawned process and communication with this.
///
/// It implements [std::io::Read] and [std::io::Write] to communicate with
/// a child.
///
/// ```no_run,ignore
/// use ptyprocess::PtyProcess;
/// use std::io::Write;
/// use std::process::Command;
///
/// let mut process = PtyProcess::spawn(Command::new("cat")).unwrap();
/// process.write_all(b"Hello World").unwrap();
/// process.flush().unwrap();
/// ```
#[derive(Debug)]
pub struct PtyProcess {
    master: Master,
    child_pid: Pid,
    stream: Stream,
    eof_char: u8,
    intr_char: u8,
    terminate_approach_delay: Duration,
}

impl PtyProcess {
    /// Spawns a child process and create a [PtyProcess].
    ///
    /// ```no_run
    ///   # use std::process::Command;
    ///   # use ptyprocess::PtyProcess;
    ///     let proc = PtyProcess::spawn(Command::new("bash"));
    /// ```
    pub fn spawn(mut command: Command) -> Result<Self> {
        let eof_char = get_eof_char();
        let intr_char = get_intr_char();

        let master = Master::open()?;
        master.grant_slave_access()?;
        master.unlock_slave()?;

        // handle errors in child executions by pipe
        let (exec_err_pipe_read, exec_err_pipe_write) = pipe()?;

        let fork = unsafe { fork()? };
        match fork {
            ForkResult::Child => {
                let err = || -> Result<()> {
                    let device = master.get_slave_name()?;
                    let slave_fd = master.get_slave_fd()?;
                    drop(master);

                    make_controlling_tty(&device)?;
                    redirect_std_streams(slave_fd)?;

                    set_echo(STDIN_FILENO, false)?;
                    set_term_size(STDIN_FILENO, DEFAULT_TERM_COLS, DEFAULT_TERM_ROWS)?;

                    close(exec_err_pipe_read)?;
                    // close pipe on sucessfull exec
                    fcntl(exec_err_pipe_write, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;

                    // Do not allow child to inherit open file descriptors from parent
                    //
                    // on linux could be used getrlimit(RLIMIT_NOFILE, rlim) interface
                    let max_open_fds = sysconf(SysconfVar::OPEN_MAX)?.unwrap() as i32;
                    // Why closing FD 1 causes an endless loop
                    (3..max_open_fds)
                        .filter(|&fd| fd != slave_fd && fd != exec_err_pipe_write)
                        .for_each(|fd| {
                            let _ = close(fd);
                        });

                    let _ = command.exec();
                    Err(Error::last())
                }()
                .unwrap_err();

                let code = err.as_errno().map_or(-1, |e| e as i32);

                write(exec_err_pipe_write, &code.to_be_bytes())?;

                process::exit(code);
            }
            ForkResult::Parent { child } => {
                close(exec_err_pipe_write)?;

                let mut pipe_buf = [0u8; 4];
                unistd::read(exec_err_pipe_read, &mut pipe_buf)?;
                let code = i32::from_be_bytes(pipe_buf);
                if code != 0 {
                    return Err(Error::from_errno(errno::from_i32(code)));
                }

                // Some systems may work in this way? (not sure)
                // that we need to set a terminal size in a parent.
                set_term_size(master.as_raw_fd(), DEFAULT_TERM_COLS, DEFAULT_TERM_ROWS)?;

                let file = master.get_file_handle()?;
                let stream = Stream::new(file);

                Ok(Self {
                    master,
                    stream,
                    child_pid: child,
                    eof_char,
                    intr_char,
                    terminate_approach_delay: Duration::from_millis(100),
                })
            }
        }
    }

    /// Returns a pid of a child process
    pub fn pid(&self) -> Pid {
        self.child_pid
    }

    /// Returns a file representation of a PTY, which can be used to communicate with it.
    ///
    /// # Safety
    ///
    /// Be carefull changing a descriptors inner state (e.g `fcntl`)
    /// because it affects all structures which use it.
    ///
    /// Be carefull using this method in async mode.
    /// Because descriptor is set to a non-blocking mode which may be unexpected.
    ///
    /// In future ut can be private for async feature if it will be considered an issue.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use ptyprocess::PtyProcess;
    /// use std::{process::Command, io::{BufReader, LineWriter}};
    ///
    /// let mut process = PtyProcess::spawn(Command::new("cat")).unwrap();
    /// let pty = process.get_pty_handle().unwrap();
    /// let mut writer = LineWriter::new(&pty);
    /// let mut reader = BufReader::new(&pty);
    /// ```
    pub fn get_pty_handle(&self) -> Result<File> {
        self.master.get_file_handle()
    }

    /// Get window size of a terminal.
    ///
    /// Default size is 80x24.
    pub fn get_window_size(&self) -> Result<(u16, u16)> {
        get_term_size(self.master.as_raw_fd())
    }

    /// Sets a terminal size.
    pub fn set_window_size(&mut self, cols: u16, rows: u16) -> Result<()> {
        set_term_size(self.master.as_raw_fd(), cols, rows)
    }

    /// Waits until a echo settings is setup.
    pub fn wait_echo(&self, on: bool, timeout: Option<Duration>) -> Result<bool> {
        let now = time::Instant::now();
        while timeout.is_none() || now.elapsed() < timeout.unwrap() {
            if on == self.get_echo()? {
                return Ok(true);
            }

            thread::sleep(Duration::from_millis(100));
        }

        Ok(false)
    }

    /// The function returns true if an echo setting is setup.
    pub fn get_echo(&self) -> Result<bool> {
        termios::tcgetattr(self.master.as_raw_fd())
            .map(|flags| flags.local_flags.contains(termios::LocalFlags::ECHO))
    }

    /// Sets a echo setting for a terminal
    pub fn set_echo(&mut self, on: bool) -> Result<()> {
        set_echo(self.master.as_raw_fd(), on)
    }

    /// Returns true if a underline `fd` connected with a TTY.
    pub fn isatty(&self) -> Result<bool> {
        isatty(self.master.as_raw_fd())
    }

    /// Set the pty process's terminate approach delay.
    pub fn set_terminate_approach_delay(&mut self, terminate_approach_delay: Duration) {
        self.terminate_approach_delay = terminate_approach_delay;
    }

    /// Status returns a status a of child process.
    pub fn status(&self) -> Result<WaitStatus> {
        waitpid(self.child_pid, Some(wait::WaitPidFlag::WNOHANG))
    }

    /// Kill sends a signal to a child process.
    ///
    /// The operation is non-blocking.
    pub fn kill(&mut self, signal: signal::Signal) -> Result<()> {
        signal::kill(self.child_pid, signal)
    }

    /// Signal is an alias to [PtyProcess::kill].
    ///
    /// [PtyProcess::kill]: struct.PtyProcess.html#method.kill
    pub fn signal(&mut self, signal: signal::Signal) -> Result<()> {
        self.kill(signal)
    }

    /// Wait blocks until a child process exits.
    ///
    /// It returns a error if the child was DEAD or not exist
    /// at the time of a call.
    ///
    /// If you need to verify that a process is dead in non-blocking way you can use
    /// [is_alive] method.
    ///
    /// [is_alive]: struct.PtyProcess.html#method.is_alive
    pub fn wait(&self) -> Result<WaitStatus> {
        waitpid(self.child_pid, None)
    }

    /// Checks if a process is still exists.
    ///
    /// It's a non blocking operation.
    ///
    /// Keep in mind that after calling this method process might be marked as DEAD by kernel,
    /// because a check of its status.
    /// Therefore second call to [Self::status] or [Self::is_alive] might return a different status.
    pub fn is_alive(&self) -> Result<bool> {
        let status = self.status();
        match status {
            Ok(status) if status == WaitStatus::StillAlive => Ok(true),
            Ok(_) | Err(Error::Sys(Errno::ECHILD)) | Err(Error::Sys(Errno::ESRCH)) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Try to force a child to terminate.
    ///
    /// This returns true if the child was terminated. and returns false if the
    /// child could not be terminated.
    ///
    /// It makes 4 tries getting more thorough.
    ///
    /// 1. SIGHUP
    /// 2. SIGCONT
    /// 3. SIGINT
    /// 4. SIGTERM
    ///
    /// If "force" is `true` then moves onto SIGKILL.
    pub fn exit(&mut self, force: bool) -> Result<bool> {
        if !self.is_alive()? {
            return Ok(true);
        }

        for &signal in &[
            signal::SIGHUP,
            signal::SIGCONT,
            signal::SIGINT,
            signal::SIGTERM,
        ] {
            if self.try_to_terminate(signal)? {
                return Ok(true);
            }
        }

        if !force {
            return Ok(false);
        }

        self.try_to_terminate(SIGKILL)
    }

    fn try_to_terminate(&mut self, signal: signal::Signal) -> Result<bool> {
        self.kill(signal)?;
        thread::sleep(self.terminate_approach_delay);

        self.is_alive().map(|is_alive| !is_alive)
    }
}

#[cfg(feature = "sync")]
impl PtyProcess {
    /// Send text to child's `STDIN`.
    ///
    /// To write bytes you can use a [std::io::Write] operations instead.
    pub fn send<S: AsRef<str>>(&mut self, s: S) -> io::Result<()> {
        self.stream.write_all(s.as_ref().as_bytes())
    }

    /// Send a line to child's `STDIN`.
    pub fn send_line<S: AsRef<str>>(&mut self, s: S) -> io::Result<()> {
        #[cfg(windows)]
        const LINE_ENDING: &[u8] = b"\r\n";
        #[cfg(not(windows))]
        const LINE_ENDING: &[u8] = b"\n";

        let bufs = &mut [
            std::io::IoSlice::new(s.as_ref().as_bytes()),
            std::io::IoSlice::new(LINE_ENDING),
            std::io::IoSlice::new(&[]), // we need to add a empty one as it may be not written.
        ];

        let _ = self.write_vectored(bufs)?;
        self.flush()?;

        Ok(())
    }

    /// Send controll character to a child process.
    ///
    /// You must be carefull passing a char or &str as an argument.
    /// If you pass an unexpected controll you'll get a error.
    /// So it may be better to use [ControlCode].
    ///
    /// ```no_run
    /// use ptyprocess::{PtyProcess, ControlCode};
    /// use std::process::Command;
    ///
    /// let mut process = PtyProcess::spawn(Command::new("cat")).unwrap();
    /// process.send_control(ControlCode::EndOfText); // sends CTRL^C
    /// process.send_control('C'); // sends CTRL^C
    /// process.send_control("^C"); // sends CTRL^C
    /// ```
    pub fn send_control(&mut self, code: impl TryInto<ControlCode>) -> io::Result<()> {
        let code = code.try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "Failed to parse a control character")
        })?;
        self.stream.write_all(&[code.into()])
    }

    /// Send `EOF` indicator to a child process.
    ///
    /// Often `eof` char handled as it would be a CTRL-C.
    pub fn send_eof(&mut self) -> io::Result<()> {
        self.stream.write_all(&[self.eof_char])
    }

    /// Send `INTR` indicator to a child process.
    ///
    /// Often `intr` char handled as it would be a CTRL-D.
    pub fn send_intr(&mut self) -> io::Result<()> {
        self.stream.write_all(&[self.intr_char])
    }

    /// Interact gives control of the child process to the interactive user (the
    /// human at the keyboard).
    ///
    /// Returns a status of a process ater interactions.
    /// Why it's crusial to return a status is after check of is_alive the actuall
    /// status might be gone.
    ///
    /// Keystrokes are sent to the child process, and
    /// the `stdout` and `stderr` output of the child process is printed.
    ///
    /// When the user types the `escape_character` this method will return control to a running process.
    /// The escape_character will not be transmitted.
    /// The default for escape_character is entered as `Ctrl-]`, the very same as BSD telnet.
    ///
    /// This simply echos the child `stdout` and `stderr` to the real `stdout` and
    /// it echos the real `stdin` to the child `stdin`.
    pub fn interact(&mut self) -> io::Result<WaitStatus> {
        // flush buffers
        self.flush()?;

        let origin_pty_echo = self.get_echo().map_err(nix_error_to_io)?;
        self.set_echo(true).map_err(nix_error_to_io)?;

        // verify: possible controlling fd can be stdout and stderr as well?
        // https://stackoverflow.com/questions/35873843/when-setting-terminal-attributes-via-tcsetattrfd-can-fd-be-either-stdout
        let isatty_in = isatty(STDIN_FILENO).map_err(nix_error_to_io)?;

        // tcgetattr issues error if a provided fd is not a tty,
        // so we run set_raw only when it's a tty.
        //
        // todo: simplify.
        if isatty_in {
            let origin_stdin_flags = termios::tcgetattr(STDIN_FILENO).map_err(nix_error_to_io)?;
            set_raw(STDIN_FILENO).map_err(nix_error_to_io)?;

            let result = self._interact();

            termios::tcsetattr(
                STDIN_FILENO,
                termios::SetArg::TCSAFLUSH,
                &origin_stdin_flags,
            )
            .map_err(nix_error_to_io)?;

            self.set_echo(origin_pty_echo).map_err(nix_error_to_io)?;

            result
        } else {
            let result = self._interact();

            self.set_echo(origin_pty_echo).map_err(nix_error_to_io)?;

            result
        }
    }

    fn _interact(&mut self) -> io::Result<WaitStatus> {
        // it's crusial to make a DUP call here.
        // If we don't actual stdin will be closed,
        // And any interaction with it may cause errors.
        //
        // Why we don't use a `std::fs::File::try_clone` with a 0 fd?
        // Because for some reason it actually doesn't make the same things as DUP does,
        // eventhough a research showed that it should.
        // https://github.com/zhiburt/expectrl/issues/7#issuecomment-884787229
        let stdin_copy_fd = dup(STDIN_FILENO).map_err(nix_error_to_io)?;
        let stdin = unsafe { std::fs::File::from_raw_fd(stdin_copy_fd) };
        let mut stdin_stream = Stream::new(stdin);

        let mut buf = [0; 512];
        loop {
            let status = self.status();
            if !matches!(status, Ok(WaitStatus::StillAlive)) {
                return status.map_err(nix_error_to_io);
            }

            let mut activity = false;

            // it prints STDIN input as well,
            // by echoing it.
            //
            // the setting must be set before calling the function.
            if let Some(n) = self.try_read(&mut buf)? {
                if n == 0 {
                    // it might be too much to call a `status()` here,
                    // do it just in case.
                    return self.status().map_err(nix_error_to_io);
                }

                std::io::stdout().write_all(&buf[..n])?;
                std::io::stdout().flush()?;

                activity = true;
            }

            if let Some(n) = stdin_stream.try_read(&mut buf)? {
                if n == 0 {
                    // it might be too much to call a `status()` here,
                    // do it just in case.
                    return self.status().map_err(nix_error_to_io);
                }

                for i in 0..n {
                    // Ctrl-]
                    if buf[i] == ControlCode::GroupSeparator.into() {
                        // it might be too much to call a `status()` here,
                        // do it just in case.
                        return self.status().map_err(nix_error_to_io);
                    }

                    self.write_all(&buf[i..i + 1])?;
                }

                activity = true;
            }

            if !activity {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

#[cfg(feature = "async")]
impl PtyProcess {
    /// Send text to child's `STDIN`.
    ///
    /// To write bytes you can use a [std::io::Write] operations instead.
    pub async fn send<S: AsRef<str>>(&mut self, s: S) -> io::Result<()> {
        self.stream.write_all(s.as_ref().as_bytes()).await
    }

    /// Send a line to child's `STDIN`.
    pub async fn send_line<S: AsRef<str>>(&mut self, s: S) -> io::Result<()> {
        #[cfg(windows)]
        const LINE_ENDING: &[u8] = b"\r\n";
        #[cfg(not(windows))]
        const LINE_ENDING: &[u8] = b"\n";

        let _ = self.write_all(s.as_ref().as_bytes()).await?;
        let _ = self.write_all(LINE_ENDING).await?;
        self.flush().await?;

        Ok(())
    }

    /// Send controll character to a child process.
    ///
    /// You must be carefull passing a char or &str as an argument.
    /// If you pass an unexpected controll you'll get a error.
    /// So it may be better to use [ControlCode].
    ///
    /// ```no_run
    /// use ptyprocess::{PtyProcess, ControlCode};
    /// use std::process::Command;
    ///
    /// let mut process = PtyProcess::spawn(Command::new("cat")).unwrap();
    /// process.send_control(ControlCode::EndOfText); // sends CTRL^C
    /// process.send_control('C'); // sends CTRL^C
    /// process.send_control("^C"); // sends CTRL^C
    /// ```
    pub async fn send_control(&mut self, code: impl TryInto<ControlCode>) -> io::Result<()> {
        let code = code.try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::Other, "Failed to parse a control character")
        })?;
        self.stream.write_all(&[code.into()]).await
    }

    /// Send `EOF` indicator to a child process.
    ///
    /// Often `eof` char handled as it would be a CTRL-C.
    pub async fn send_eof(&mut self) -> io::Result<()> {
        self.stream.write_all(&[self.eof_char]).await
    }

    /// Send `INTR` indicator to a child process.
    ///
    /// Often `intr` char handled as it would be a CTRL-D.
    pub async fn send_intr(&mut self) -> io::Result<()> {
        self.stream.write_all(&[self.intr_char]).await
    }

    /// Interact gives control of the child process to the interactive user (the
    /// human at the keyboard).
    ///
    /// Returns a status of a process ater interactions.
    /// Why it's crusial to return a status is after check of is_alive the actuall
    /// status might be gone.
    ///
    /// Keystrokes are sent to the child process, and
    /// the `stdout` and `stderr` output of the child process is printed.
    ///
    /// When the user types the `escape_character` this method will return control to a running process.
    /// The escape_character will not be transmitted.
    /// The default for escape_character is entered as `Ctrl-]`, the very same as BSD telnet.
    ///
    /// This simply echos the child `stdout` and `stderr` to the real `stdout` and
    /// it echos the real `stdin` to the child `stdin`.
    pub async fn interact(&mut self) -> io::Result<WaitStatus> {
        // flush buffers
        self.flush().await?;

        let origin_pty_echo = self.get_echo().map_err(nix_error_to_io)?;
        self.set_echo(true).map_err(nix_error_to_io)?;

        // verify: possible controlling fd can be stdout and stderr as well?
        // https://stackoverflow.com/questions/35873843/when-setting-terminal-attributes-via-tcsetattrfd-can-fd-be-either-stdout
        let isatty_in = isatty(STDIN_FILENO).map_err(nix_error_to_io)?;

        // tcgetattr issues error if a provided fd is not a tty,
        // so we run set_raw only when it's a tty.
        //
        // todo: simplify.
        if isatty_in {
            let origin_stdin_flags = termios::tcgetattr(STDIN_FILENO).map_err(nix_error_to_io)?;
            set_raw(STDIN_FILENO).map_err(nix_error_to_io)?;

            let result = self._interact().await;

            termios::tcsetattr(
                STDIN_FILENO,
                termios::SetArg::TCSAFLUSH,
                &origin_stdin_flags,
            )
            .map_err(nix_error_to_io)?;

            self.set_echo(origin_pty_echo).map_err(nix_error_to_io)?;

            result
        } else {
            let result = self._interact().await;

            self.set_echo(origin_pty_echo).map_err(nix_error_to_io)?;

            result
        }
    }

    async fn _interact(&mut self) -> io::Result<WaitStatus> {
        // it's crusial to make a DUP call here.
        // If we don't actual stdin will be closed,
        // And any interaction with it may cause errors.
        //
        // Why we don't use a `std::fs::File::try_clone` with a 0 fd?
        // Because for some reason it actually doesn't make the same things as DUP does,
        // eventhough a research showed that it should.
        // https://github.com/zhiburt/expectrl/issues/7#issuecomment-884787229
        let stdin_copy_fd = dup(0).map_err(nix_error_to_io)?;

        let stdin = unsafe { std::fs::File::from_raw_fd(stdin_copy_fd) };
        let mut stdin_stream = Stream::new(stdin);

        let mut buf = [0; 512];
        loop {
            let status = self.status();
            if !matches!(status, Ok(WaitStatus::StillAlive)) {
                return status.map_err(nix_error_to_io);
            }

            // it prints STDIN input as well,
            // by echoing it.
            //
            // the setting must be set before calling the function.
            if let Some(n) = self.try_read(&mut buf).await? {
                std::io::stdout().write_all(&buf[..n])?;
                std::io::stdout().flush()?;
            }

            if let Some(n) = stdin_stream.try_read(&mut buf).await? {
                for i in 0..n {
                    // Ctrl-]
                    if buf[i] == ControlCode::GroupSeparator.into() {
                        // it might be too much to call a `status()` here,
                        // do it just in case.
                        return self.status().map_err(nix_error_to_io);
                    }

                    self.write_all(&buf[i..i + 1]).await?;
                }
            }
        }
    }
}

fn nix_error_to_io(err: nix::Error) -> io::Error {
    match err.as_errno() {
        Some(code) => io::Error::from_raw_os_error(code as _),
        None => io::Error::new(
            io::ErrorKind::Other,
            "Unexpected error type conversion from nix to io",
        ),
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if let Ok(WaitStatus::StillAlive) = self.status() {
            self.exit(true).unwrap();
        }
    }
}

impl Deref for PtyProcess {
    type Target = Stream;

    fn deref(&self) -> &Self::Target {
        &self.stream
    }
}

impl DerefMut for PtyProcess {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.stream
    }
}

fn set_term_size(fd: i32, cols: u16, rows: u16) -> Result<()> {
    ioctl_write_ptr_bad!(_set_window_size, libc::TIOCSWINSZ, winsize);

    let size = winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let _ = unsafe { _set_window_size(fd, &size) }?;

    Ok(())
}

fn get_term_size(fd: i32) -> Result<(u16, u16)> {
    nix::ioctl_read_bad!(_get_window_size, libc::TIOCGWINSZ, winsize);

    let mut size = winsize {
        ws_col: 0,
        ws_row: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let _ = unsafe { _get_window_size(fd, &mut size) }?;

    Ok((size.ws_col, size.ws_row))
}

#[derive(Debug)]
struct Master {
    fd: PtyMaster,
}

impl Master {
    fn open() -> Result<Self> {
        let master_fd = posix_openpt(OFlag::O_RDWR)?;
        Ok(Self { fd: master_fd })
    }

    fn grant_slave_access(&self) -> Result<()> {
        grantpt(&self.fd)
    }

    fn unlock_slave(&self) -> Result<()> {
        unlockpt(&self.fd)
    }

    fn get_slave_name(&self) -> Result<String> {
        get_slave_name(&self.fd)
    }

    fn get_slave_fd(&self) -> Result<RawFd> {
        let slave_name = self.get_slave_name()?;
        let slave_fd = open(slave_name.as_str(), OFlag::O_RDWR, Mode::empty())?;
        Ok(slave_fd)
    }

    fn get_file_handle(&self) -> Result<File> {
        let fd = dup(self.as_raw_fd())?;
        let file = unsafe { File::from_raw_fd(fd) };

        Ok(file)
    }
}

impl AsRawFd for Master {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

#[cfg(not(target_os = "macos"))]
fn get_slave_name(fd: &PtyMaster) -> Result<String> {
    nix::pty::ptsname_r(fd)
}

/// Getting a slave name on darvin platform
/// https://blog.tarq.io/ptsname-on-osx-with-rust/
#[cfg(target_os = "macos")]
fn get_slave_name(fd: &PtyMaster) -> Result<String> {
    use nix::libc::ioctl;
    use nix::libc::TIOCPTYGNAME;
    use std::ffi::CStr;
    use std::os::raw::c_char;
    use std::os::unix::prelude::AsRawFd;

    // ptsname_r is a linux extension but ptsname isn't thread-safe
    // we could use a static mutex but instead we re-implemented ptsname_r with a syscall
    // ioctl(fd, TIOCPTYGNAME, buf) manually
    // the buffer size on OSX is 128, defined by sys/ttycom.h
    let mut buf: [c_char; 128] = [0; 128];

    let fd = fd.as_raw_fd();

    match unsafe { ioctl(fd, TIOCPTYGNAME as u64, &mut buf) } {
        0 => {
            let string = unsafe { CStr::from_ptr(buf.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            return Ok(string);
        }
        _ => Err(Error::last()),
    }
}

fn redirect_std_streams(fd: RawFd) -> Result<()> {
    // If fildes2 is already a valid open file descriptor, it shall be closed first

    close(STDIN_FILENO)?;
    close(STDOUT_FILENO)?;
    close(STDERR_FILENO)?;

    // use slave fd as std[in/out/err]
    dup2(fd, STDIN_FILENO)?;
    dup2(fd, STDOUT_FILENO)?;
    dup2(fd, STDERR_FILENO)?;

    Ok(())
}

fn set_echo(fd: RawFd, on: bool) -> Result<()> {
    // Set echo off
    // Even though there may be something left behind https://stackoverflow.com/a/59034084
    let mut flags = termios::tcgetattr(fd)?;
    match on {
        true => flags.local_flags |= termios::LocalFlags::ECHO,
        false => flags.local_flags &= !termios::LocalFlags::ECHO,
    }

    termios::tcsetattr(fd, termios::SetArg::TCSANOW, &flags)?;
    Ok(())
}

fn set_raw(fd: RawFd) -> Result<()> {
    let mut flags = termios::tcgetattr(fd)?;

    #[cfg(not(target_os = "macos"))]
    {
        termios::cfmakeraw(&mut flags);
    }
    #[cfg(target_os = "macos")]
    {
        // implementation is taken from https://github.com/python/cpython/blob/3.9/Lib/tty.py
        use nix::libc::{VMIN, VTIME};
        use termios::ControlFlags;
        use termios::InputFlags;
        use termios::LocalFlags;
        use termios::OutputFlags;

        flags.input_flags &= !(InputFlags::BRKINT
            | InputFlags::ICRNL
            | InputFlags::INPCK
            | InputFlags::ISTRIP
            | InputFlags::IXON);
        flags.output_flags &= !OutputFlags::OPOST;
        flags.control_flags &= !(ControlFlags::CSIZE | ControlFlags::PARENB);
        flags.control_flags |= ControlFlags::CS8;
        flags.local_flags &=
            !(LocalFlags::ECHO | LocalFlags::ICANON | LocalFlags::IEXTEN | LocalFlags::ISIG);
        flags.control_chars[VMIN] = 1;
        flags.control_chars[VTIME] = 0;
    }

    termios::tcsetattr(fd, termios::SetArg::TCSANOW, &flags)?;
    Ok(())
}

fn get_this_term_char(char: SpecialCharacterIndices) -> Option<u8> {
    for &fd in &[STDIN_FILENO, STDOUT_FILENO] {
        if let Ok(char) = get_term_char(fd, char) {
            return Some(char);
        }
    }

    None
}

fn get_intr_char() -> u8 {
    get_this_term_char(SpecialCharacterIndices::VINTR).unwrap_or(DEFAULT_INTR_CHAR)
}

fn get_eof_char() -> u8 {
    get_this_term_char(SpecialCharacterIndices::VEOF).unwrap_or(DEFAULT_VEOF_CHAR)
}

fn get_term_char(fd: RawFd, char: SpecialCharacterIndices) -> Result<u8> {
    let flags = termios::tcgetattr(fd)?;
    let b = flags.control_chars[char as usize];
    Ok(b)
}

fn make_controlling_tty(child_name: &str) -> Result<()> {
    // Is this appoach's result the same as just call ioctl TIOCSCTTY?

    // Disconnect from controlling tty, if any
    let fd = open("/dev/tty", OFlag::O_RDWR | OFlag::O_NOCTTY, Mode::empty());
    match fd {
        Ok(fd) => {
            close(fd)?;
        }
        Err(Error::Sys(Errno::ENXIO)) => {
            // Sometimes we get ENXIO right here which 'probably' means
            // that we has been already disconnected from controlling tty.
            // Specifically it was discovered on ubuntu-latest Github CI platform.
        }
        Err(err) => return Err(err),
    }

    setsid()?;

    // Verify we are disconnected from controlling tty by attempting to open
    // it again.  We expect that OSError of ENXIO should always be raised.
    let fd = open("/dev/tty", OFlag::O_RDWR | OFlag::O_NOCTTY, Mode::empty());
    match fd {
        Err(Error::Sys(Errno::ENXIO)) => {} // ok
        Ok(fd) => {
            close(fd)?;
            return Err(Error::UnsupportedOperation);
        }
        Err(_) => return Err(Error::UnsupportedOperation),
    }

    // Verify we can open child pty.
    let fd = open(child_name, OFlag::O_RDWR, Mode::empty())?;
    close(fd)?;

    // Verify we now have a controlling tty.
    let fd = open("/dev/tty", OFlag::O_WRONLY, Mode::empty())?;
    close(fd)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_pty() -> Result<()> {
        let master = Master::open()?;
        master.grant_slave_access()?;
        master.unlock_slave()?;
        let slavename = master.get_slave_name()?;
        assert!(slavename.starts_with("/dev"));
        println!("slave name {}", slavename);
        Ok(())
    }

    #[test]
    #[ignore = "The test should be run in a sigle thread mode --jobs 1 or --test-threads 1"]
    fn release_pty_master() -> Result<()> {
        let master = Master::open()?;
        let old_master_fd = master.fd.as_raw_fd();

        drop(master);

        let master = Master::open()?;

        assert!(master.fd.as_raw_fd() == old_master_fd);

        Ok(())
    }
}
