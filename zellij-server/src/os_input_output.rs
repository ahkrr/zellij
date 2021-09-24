use std::collections::HashMap;
use std::env;
use std::os::unix::io::RawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};

use zellij_utils::{async_std, interprocess, libc, nix, signal_hook, zellij_tile};

use async_std::fs::File as AsyncFile;
use async_std::os::unix::io::FromRawFd;
use interprocess::local_socket::LocalSocketStream;
use nix::pty::{forkpty, Winsize};
use nix::sys::signal::{kill, Signal};
use nix::sys::termios;
use nix::sys::wait::waitpid;
use nix::unistd::{self, ForkResult};
use signal_hook::consts::*;
use zellij_tile::data::Palette;
use zellij_utils::{
    input::command::{RunCommand, TerminalAction},
    ipc::{ClientToServerMsg, IpcReceiverWithContext, IpcSenderWithContext, ServerToClientMsg},
    shared::default_palette,
};

use async_std::io::ReadExt;
pub use async_trait::async_trait;

pub use nix::unistd::Pid;

use crate::ClientId;

pub(crate) fn set_terminal_size_using_fd(fd: RawFd, columns: u16, rows: u16) {
    // TODO: do this with the nix ioctl
    use libc::ioctl;
    use libc::TIOCSWINSZ;

    let winsize = Winsize {
        ws_col: columns,
        ws_row: rows,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // TIOCGWINSZ is an u32, but the second argument to ioctl is u64 on
    // some platforms. When checked on Linux, clippy will complain about
    // useless conversion.
    #[allow(clippy::useless_conversion)]
    unsafe {
        ioctl(fd, TIOCSWINSZ.into(), &winsize)
    };
}

/// Handle some signals for the child process. This will loop until the child
/// process exits.
fn handle_command_exit(mut child: Child) {
    let mut should_exit = false;
    let mut attempts = 3;
    let mut signals = signal_hook::iterator::Signals::new(&[SIGINT, SIGTERM]).unwrap();
    'handle_exit: loop {
        // test whether the child process has exited
        match child.try_wait() {
            Ok(Some(_status)) => {
                // if the child process has exited, break outside of the loop
                // and exit this function
                // TODO: handle errors?
                break 'handle_exit;
            }
            Ok(None) => {
                ::std::thread::sleep(::std::time::Duration::from_millis(10));
            }
            Err(e) => panic!("error attempting to wait: {}", e),
        }

        if !should_exit {
            for signal in signals.pending() {
                if signal == SIGINT || signal == SIGTERM {
                    should_exit = true;
                }
            }
        } else if attempts > 0 {
            // let's try nicely first...
            attempts -= 1;
            kill(Pid::from_raw(child.id() as i32), Some(Signal::SIGTERM)).unwrap();
            continue;
        } else {
            // when I say whoa, I mean WHOA!
            let _ = child.kill();
            break 'handle_exit;
        }
    }
}

/// Spawns a new terminal from the parent terminal with [`termios`](termios::Termios)
/// `orig_termios`.
///
fn handle_terminal(cmd: RunCommand, orig_termios: termios::Termios) -> (RawFd, Pid) {
    let (pid_primary, pid_secondary): (RawFd, Pid) = {
        match forkpty(None, Some(&orig_termios)) {
            Ok(fork_pty_res) => {
                let pid_primary = fork_pty_res.master;
                let pid_secondary = match fork_pty_res.fork_result {
                    ForkResult::Parent { child } => child,
                    ForkResult::Child => {
                        let child = unsafe {
                            Command::new(cmd.command)
                                .args(&cmd.args)
                                .pre_exec(|| -> std::io::Result<()> {
                                    // this is the "unsafe" part, for more details please see:
                                    // https://doc.rust-lang.org/std/os/unix/process/trait.CommandExt.html#notes-and-safety
                                    unistd::setpgid(Pid::from_raw(0), Pid::from_raw(0))
                                        .expect("failed to create a new process group");
                                    Ok(())
                                })
                                .spawn()
                                .expect("failed to spawn")
                        };
                        unistd::tcsetpgrp(0, Pid::from_raw(child.id() as i32))
                            .expect("faled to set child's forceground process group");
                        handle_command_exit(child);
                        ::std::process::exit(0);
                    }
                };
                (pid_primary, pid_secondary)
            }
            Err(e) => {
                panic!("failed to fork {:?}", e);
            }
        }
    };
    (pid_primary, pid_secondary)
}

/// If a [`TerminalAction::OpenFile(file)`] is given, the text editor specified by environment variable `EDITOR`
/// (or `VISUAL`, if `EDITOR` is not set) will be started in the new terminal, with the given
/// file open.
/// If [`TerminalAction::RunCommand(RunCommand)`] is given, the command will be started
/// in the new terminal.
/// If None is given, the shell specified by environment variable `SHELL` will
/// be started in the new terminal.
///
/// # Panics
///
/// This function will panic if both the `EDITOR` and `VISUAL` environment variables are not
/// set.
pub fn spawn_terminal(
    terminal_action: Option<TerminalAction>,
    orig_termios: termios::Termios,
) -> (RawFd, Pid) {
    let cmd = match terminal_action {
        Some(TerminalAction::OpenFile(file_to_open)) => {
            if env::var("EDITOR").is_err() && env::var("VISUAL").is_err() {
                panic!("Can't edit files if an editor is not defined. To fix: define the EDITOR or VISUAL environment variables with the path to your editor (eg. /usr/bin/vim)");
            }
            let command =
                PathBuf::from(env::var("EDITOR").unwrap_or_else(|_| env::var("VISUAL").unwrap()));

            let args = vec![file_to_open
                .into_os_string()
                .into_string()
                .expect("Not valid Utf8 Encoding")];
            RunCommand { command, args }
        }
        Some(TerminalAction::RunCommand(command)) => command,
        None => {
            let command =
                PathBuf::from(env::var("SHELL").expect("Could not find the SHELL variable"));
            let args = vec![];
            RunCommand { command, args }
        }
    };

    handle_terminal(cmd, orig_termios)
}

#[derive(Clone)]
pub struct ServerOsInputOutput {
    orig_termios: Arc<Mutex<termios::Termios>>,
    client_senders: Arc<Mutex<HashMap<ClientId, IpcSenderWithContext<ServerToClientMsg>>>>,
}

// async fn in traits is not supported by rust, so dtolnay's excellent async_trait macro is being
// used. See https://smallcultfollowing.com/babysteps/blog/2019/10/26/async-fn-in-traits-are-hard/
#[async_trait]
pub trait AsyncReader: Send + Sync {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error>;
}

/// An `AsyncReader` that wraps a `RawFd`
struct RawFdAsyncReader {
    fd: async_std::fs::File,
}

impl RawFdAsyncReader {
    fn new(fd: RawFd) -> RawFdAsyncReader {
        RawFdAsyncReader {
            /// The supplied `RawFd` is consumed by the created `RawFdAsyncReader`, closing it when dropped
            fd: unsafe { AsyncFile::from_raw_fd(fd) },
        }
    }
}

#[async_trait]
impl AsyncReader for RawFdAsyncReader {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        self.fd.read(buf).await
    }
}

/// The `ServerOsApi` trait represents an abstract interface to the features of an operating system that
/// Zellij server requires.
pub trait ServerOsApi: Send + Sync {
    /// Sets the size of the terminal associated to file descriptor `fd`.
    fn set_terminal_size_using_fd(&self, fd: RawFd, cols: u16, rows: u16);
    /// Spawn a new terminal, with a terminal action.
    fn spawn_terminal(&self, terminal_action: Option<TerminalAction>) -> (RawFd, Pid);
    /// Read bytes from the standard output of the virtual terminal referred to by `fd`.
    fn read_from_tty_stdout(&self, fd: RawFd, buf: &mut [u8]) -> Result<usize, nix::Error>;
    /// Creates an `AsyncReader` that can be used to read from `fd` in an async context
    fn async_file_reader(&self, fd: RawFd) -> Box<dyn AsyncReader>;
    /// Write bytes to the standard input of the virtual terminal referred to by `fd`.
    fn write_to_tty_stdin(&self, fd: RawFd, buf: &[u8]) -> Result<usize, nix::Error>;
    /// Wait until all output written to the object referred to by `fd` has been transmitted.
    fn tcdrain(&self, fd: RawFd) -> Result<(), nix::Error>;
    /// Terminate the process with process ID `pid`. (SIGTERM)
    fn kill(&self, pid: Pid) -> Result<(), nix::Error>;
    /// Terminate the process with process ID `pid`. (SIGKILL)
    fn force_kill(&self, pid: Pid) -> Result<(), nix::Error>;
    /// Returns a [`Box`] pointer to this [`ServerOsApi`] struct.
    fn box_clone(&self) -> Box<dyn ServerOsApi>;
    fn send_to_client(&self, client_id: ClientId, msg: ServerToClientMsg);
    fn new_client(
        &mut self,
        client_id: ClientId,
        stream: LocalSocketStream,
    ) -> IpcReceiverWithContext<ClientToServerMsg>;
    fn remove_client(&mut self, client_id: ClientId);
    fn load_palette(&self) -> Palette;
}

impl ServerOsApi for ServerOsInputOutput {
    fn set_terminal_size_using_fd(&self, fd: RawFd, cols: u16, rows: u16) {
        if cols > 0 && rows > 0 {
            set_terminal_size_using_fd(fd, cols, rows);
        }
    }
    fn spawn_terminal(&self, terminal_action: Option<TerminalAction>) -> (RawFd, Pid) {
        let orig_termios = self.orig_termios.lock().unwrap();
        spawn_terminal(terminal_action, orig_termios.clone())
    }
    fn read_from_tty_stdout(&self, fd: RawFd, buf: &mut [u8]) -> Result<usize, nix::Error> {
        unistd::read(fd, buf)
    }
    fn async_file_reader(&self, fd: RawFd) -> Box<dyn AsyncReader> {
        Box::new(RawFdAsyncReader::new(fd))
    }
    fn write_to_tty_stdin(&self, fd: RawFd, buf: &[u8]) -> Result<usize, nix::Error> {
        unistd::write(fd, buf)
    }
    fn tcdrain(&self, fd: RawFd) -> Result<(), nix::Error> {
        termios::tcdrain(fd)
    }
    fn box_clone(&self) -> Box<dyn ServerOsApi> {
        Box::new((*self).clone())
    }
    fn kill(&self, pid: Pid) -> Result<(), nix::Error> {
        kill(pid, Some(Signal::SIGTERM)).unwrap();
        waitpid(pid, None).unwrap();
        Ok(())
    }
    fn force_kill(&self, pid: Pid) -> Result<(), nix::Error> {
        let _ = kill(pid, Some(Signal::SIGKILL));
        Ok(())
    }
    fn send_to_client(&self, client_id: ClientId, msg: ServerToClientMsg) {
        if let Some(sender) = self.client_senders.lock().unwrap().get_mut(&client_id) {
            sender.send(msg);
        }
    }
    fn new_client(
        &mut self,
        client_id: ClientId,
        stream: LocalSocketStream,
    ) -> IpcReceiverWithContext<ClientToServerMsg> {
        let receiver = IpcReceiverWithContext::new(stream);
        let sender = receiver.get_sender();
        self.client_senders
            .lock()
            .unwrap()
            .insert(client_id, sender);
        receiver
    }
    fn remove_client(&mut self, client_id: ClientId) {
        let mut client_senders = self.client_senders.lock().unwrap();
        if client_senders.contains_key(&client_id) {
            client_senders.remove(&client_id);
        }
    }
    fn load_palette(&self) -> Palette {
        default_palette()
    }
}

impl Clone for Box<dyn ServerOsApi> {
    fn clone(&self) -> Box<dyn ServerOsApi> {
        self.box_clone()
    }
}

pub fn get_server_os_input() -> Result<ServerOsInputOutput, nix::Error> {
    let current_termios = termios::tcgetattr(0)?;
    let orig_termios = Arc::new(Mutex::new(current_termios));
    Ok(ServerOsInputOutput {
        orig_termios,
        client_senders: Arc::new(Mutex::new(HashMap::new())),
    })
}
