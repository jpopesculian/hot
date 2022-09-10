use ansi_term::Style;
use crossterm::{
    event::{poll, read, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal,
};
use mio::{unix::SourceFd, Events, Interest, Poll, Registry, Token};
use std::{
    io::{self, ErrorKind, Read, Result, Write},
    ops,
    os::unix::prelude::AsRawFd,
    panic,
    process::{Child, Command, Stdio},
    time::Duration,
};

fn usage() {
    println!(
        r#"USAGE

hot [OPTIONS..] [CMD] [ARGS..]

OPTIONS

--help    Display this message

DESCRIPTION

Helper to make commands reloadable. When running press 'r' to reload
and ctrl^c or ctrl^d to quit."#
    );
}

fn parse_args() -> (String, Vec<String>) {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() {
        usage();
        std::process::exit(1);
    } else if args.len() == 1 && matches!(args[0].to_lowercase().as_str(), "-h" | "--help") {
        usage();
        std::process::exit(0);
    }
    let cmd = args.remove(0);
    (cmd, args)
}

fn wrap_raw_mode<F, T>(mut func: F) -> Result<T>
where
    F: FnMut(bool) -> Result<T>,
{
    let should_disable = if terminal::is_raw_mode_enabled()? {
        false
    } else {
        terminal::enable_raw_mode()?;
        let default_hook = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            let _ = terminal::disable_raw_mode();
            default_hook(info)
        }));
        true
    };
    let res = func(should_disable);
    let disable_res = if should_disable {
        let _ = panic::take_hook();
        terminal::disable_raw_mode()
    } else {
        Ok(())
    };
    res.and_then(|ret| {
        disable_res?;
        Ok(ret)
    })
}

fn read_reload_event() -> Result<bool> {
    wrap_raw_mode(|should_disable| {
        if poll(Duration::from_secs(0))? {
            match read()? {
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c' | 'd'),
                    modifiers: KeyModifiers::CONTROL,
                    kind: KeyEventKind::Press,
                    ..
                }) => {
                    if should_disable {
                        terminal::disable_raw_mode()?;
                    }
                    std::process::exit(2);
                }
                Event::Key(KeyEvent {
                    code: KeyCode::Char('r' | 'R'),
                    modifiers: KeyModifiers::NONE | KeyModifiers::SHIFT,
                    kind: KeyEventKind::Press,
                    ..
                }) => Ok(true),
                _ => Ok(false),
            }
        } else {
            Ok(false)
        }
    })
}

pub struct Pipe(Vec<u8>);

impl Pipe {
    fn with_capacity(capacity: usize) -> Self {
        Self(vec![0; capacity])
    }

    fn transfer<R: Read, W: Write>(&mut self, reader: &mut R, writer: &mut W) -> io::Result<()> {
        let read = reader.read(&mut self.0)?;
        writer.write_all(&self.0[..read])
    }
}

pub struct Process(Child);

impl Process {
    const STDOUT: Token = Token(0);
    const STDERR: Token = Token(1);

    fn spawn(cmd: &str, args: &[String]) -> Result<Self> {
        eprintln!(
            "{}",
            Style::new()
                .bold()
                .paint(format!("{} {}", cmd, args.join(" ")))
        );
        Ok(Self(
            Command::new(cmd)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        ))
    }

    fn register(&self, registry: &Registry) -> Result<()> {
        registry.register(
            &mut SourceFd(&self.stdout.as_ref().unwrap().as_raw_fd()),
            Self::STDOUT,
            Interest::READABLE,
        )?;
        registry.register(
            &mut SourceFd(&self.stderr.as_ref().unwrap().as_raw_fd()),
            Self::STDERR,
            Interest::READABLE,
        )?;
        Ok(())
    }

    fn deregister(&self, registry: &Registry) -> Result<()> {
        registry.deregister(&mut SourceFd(&self.stdout.as_ref().unwrap().as_raw_fd()))?;
        registry.deregister(&mut SourceFd(&self.stderr.as_ref().unwrap().as_raw_fd()))?;
        Ok(())
    }
}

impl ops::Deref for Process {
    type Target = Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ops::DerefMut for Process {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

fn main() -> Result<()> {
    let (cmd, args) = parse_args();
    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(128);

    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let mut pipe = Pipe::with_capacity(4096);

    let mut process = Process::spawn(&cmd, &args)?;
    process.register(poll.registry())?;
    loop {
        if read_reload_event()? {
            eprintln!("{}", Style::new().bold().paint("[RELOAD]"));
            process.deregister(poll.registry())?;
            process.kill()?;
            let _ = process.wait()?;
            process = Process::spawn(&cmd, &args)?;
            process.register(poll.registry())?;
        }

        if let Err(err) = poll.poll(&mut events, Some(Duration::from_millis(100))) {
            if err.kind() != ErrorKind::Interrupted {
                return Err(err);
            }
        }
        for event in events.iter() {
            match event.token() {
                Process::STDERR => {
                    if event.is_readable() {
                        pipe.transfer(process.stderr.as_mut().unwrap(), &mut stderr)?;
                    }
                }
                Process::STDOUT => {
                    if event.is_readable() {
                        pipe.transfer(process.stdout.as_mut().unwrap(), &mut stdout)?;
                    }
                }
                _ => {}
            }
        }
        events.clear();

        if let Some(exit_status) = process.try_wait()? {
            std::process::exit(exit_status.code().unwrap_or(11));
        }
    }
}
