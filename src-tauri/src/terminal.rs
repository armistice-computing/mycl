use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex, MutexGuard, Weak,
    },
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const SETTINGS_FILE: &str = "terminal.json";
const SCHEMA_VERSION: u32 = 1;
const REPLAY_BYTES: usize = 256 * 1024;
const READ_CHUNK_BYTES: usize = 16 * 1024;
/// Output that arrives within this window goes to the page as one frame.
const FRAME_WINDOW: Duration = Duration::from_millis(8);
const FRAME_BYTES: usize = 128 * 1024;
/// Chunks the reader may hold ahead of the page before the pty fills and
/// the program writing to it waits.
const PENDING_CHUNKS: usize = 32;
/// The most output the page is sent per second. The page cannot say "wait",
/// so the pause is taken here: a flood (`yes`, `cat` of a large file) backs
/// up into the pty and the program writing it waits, as it would on a slow
/// terminal. Ordinary output is far below this and is not delayed.
const PAGE_BYTES_PER_SECOND: u64 = 4 * 1024 * 1024;
/// How long the last output may trail the shell's exit before Exit is sent.
const EXIT_DRAIN: Duration = Duration::from_millis(500);
/// Time a hung-up shell has to save its history and leave before SIGKILL.
const END_GRACE: Duration = Duration::from_secs(2);
const QUIT_GRACE: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalEvent {
    Data,
    Exit(i32),
}

pub(crate) type TerminalFrameSink = Arc<dyn Fn(&str, TerminalEvent, &[u8]) + Send + Sync>;

#[derive(Debug, Clone)]
pub(crate) struct TerminalOpenResult {
    pub session: String,
    pub generation: u64,
    pub replay: Vec<u8>,
}

#[derive(Debug, Clone)]
struct SpawnConfig {
    program: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TerminalSettingsFile {
    schema_version: u32,
    enabled: bool,
}

fn settings_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join(SETTINGS_FILE)
}

fn read_settings(profile_dir: &Path) -> Result<bool, String> {
    let path = settings_path(profile_dir);
    if !path.exists() {
        return Ok(false);
    }
    let file: TerminalSettingsFile =
        crate::storage::read_json(&path).map_err(|error| error.to_string())?;
    if file.schema_version != SCHEMA_VERSION {
        return Err(format!(
            "unsupported terminal settings version {}",
            file.schema_version
        ));
    }
    Ok(file.enabled)
}

fn write_settings(profile_dir: &Path, enabled: bool) -> Result<(), String> {
    fs::create_dir_all(profile_dir)
        .map_err(|error| format!("create terminal settings directory: {error}"))?;
    crate::storage::write_json(
        &settings_path(profile_dir),
        &TerminalSettingsFile {
            schema_version: SCHEMA_VERSION,
            enabled,
        },
    )
    .map_err(|error| error.to_string())
}

fn default_spawn_config() -> Result<SpawnConfig, String> {
    let fallback = if cfg!(target_os = "macos") {
        "/bin/zsh"
    } else {
        "/bin/bash"
    };
    let program = env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_file())
        .unwrap_or_else(|| PathBuf::from(fallback));
    let cwd = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .ok_or_else(|| "terminal could not determine the user's home directory".to_string())?;
    Ok(SpawnConfig {
        program,
        args: vec!["-l".to_string()],
        cwd,
    })
}

/// Variables the shell takes from the app's environment: who the person is,
/// their language, and the handles to their desktop session.
fn shell_may_inherit(name: &str) -> bool {
    matches!(
        name,
        "HOME"
            | "USER"
            | "LOGNAME"
            | "SHELL"
            | "PATH"
            | "LANG"
            | "LANGUAGE"
            | "TZ"
            | "TMPDIR"
            | "SSH_AUTH_SOCK"
            | "DISPLAY"
            | "WAYLAND_DISPLAY"
            | "DBUS_SESSION_BUS_ADDRESS"
            | "XAUTHORITY"
            | "DESKTOP_SESSION"
            | "GNOME_KEYRING_CONTROL"
            | "__CF_USER_TEXT_ENCODING"
    ) || matches!(
        // On Linux these come from the desktop session, not from a profile.
        name.to_ascii_lowercase().as_str(),
        "http_proxy" | "https_proxy" | "all_proxy" | "no_proxy"
    ) || name.starts_with("LC_")
        || name.starts_with("XDG_")
}

fn pty_size(cols: u16, rows: u16) -> Result<PtySize, String> {
    if cols == 0 || rows == 0 {
        return Err("terminal dimensions must be greater than zero".to_string());
    }
    Ok(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })
}

struct ReplayState {
    bytes: Vec<u8>,
    sink: Option<TerminalFrameSink>,
}

struct Session {
    id: String,
    generation: u64,
    process_group: u32,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    replay: Mutex<ReplayState>,
    alive: AtomicBool,
    exited: AtomicBool,
}

struct ManagerInner {
    active: Option<Arc<Session>>,
    generation: u64,
}

#[derive(Clone)]
pub(crate) struct TerminalManager {
    profile_dir: PathBuf,
    spawn: Option<SpawnConfig>,
    enabled: Arc<AtomicBool>,
    inner: Arc<Mutex<ManagerInner>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl TerminalManager {
    pub fn new(profile_dir: PathBuf) -> Result<Self, String> {
        // A settings file that cannot be read means the terminal is off; it
        // must never keep the app from starting.
        let enabled = read_settings(&profile_dir).unwrap_or_else(|error| {
            eprintln!("terminal settings unreadable, terminal stays off: {error}");
            false
        });
        Ok(Self {
            profile_dir,
            spawn: default_spawn_config().ok(),
            enabled: Arc::new(AtomicBool::new(enabled)),
            inner: Arc::new(Mutex::new(ManagerInner {
                active: None,
                generation: 0,
            })),
        })
    }

    #[cfg(test)]
    fn with_spawn(profile_dir: PathBuf, spawn: SpawnConfig) -> Self {
        Self {
            profile_dir,
            spawn: Some(spawn),
            enabled: Arc::new(AtomicBool::new(false)),
            inner: Arc::new(Mutex::new(ManagerInner {
                active: None,
                generation: 0,
            })),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        write_settings(&self.profile_dir, enabled)?;
        self.enabled.store(enabled, Ordering::SeqCst);
        if !enabled {
            self.end_active(None);
        }
        Ok(())
    }

    pub fn open(
        &self,
        cols: u16,
        rows: u16,
        sink: TerminalFrameSink,
    ) -> Result<TerminalOpenResult, String> {
        if !self.enabled() {
            return Err("terminal is disabled".to_string());
        }
        let size = pty_size(cols, rows)?;
        let existing = {
            let inner = lock(&self.inner);
            inner
                .active
                .as_ref()
                .filter(|session| session.alive.load(Ordering::SeqCst))
                .cloned()
        };
        if let Some(session) = existing {
            lock(&session.master)
                .resize(size)
                .map_err(|error| format!("resize terminal: {error}"))?;
            let mut replay = lock(&session.replay);
            replay.sink = Some(sink);
            return Ok(TerminalOpenResult {
                session: session.id.clone(),
                generation: session.generation,
                replay: replay.bytes.clone(),
            });
        }

        let (session, reader) = self.spawn_session(size, sink)?;
        let result = TerminalOpenResult {
            session: session.id.clone(),
            generation: session.generation,
            replay: Vec::new(),
        };
        let mut inner = lock(&self.inner);
        if let Some(active) = inner.active.take() {
            if active.alive.load(Ordering::SeqCst) {
                Self::end(active, None);
            }
        }
        inner.active = Some(session.clone());
        drop(inner);
        let drained = Self::start_reader(session.clone(), reader);
        Self::start_waiter(session, Arc::downgrade(&self.inner), drained);
        Ok(result)
    }

    fn spawn_session(
        &self,
        size: PtySize,
        sink: TerminalFrameSink,
    ) -> Result<(Arc<Session>, Box<dyn Read + Send>), String> {
        let spawn = match &self.spawn {
            Some(spawn) => spawn.clone(),
            None => default_spawn_config()?,
        };
        let pty = native_pty_system()
            .openpty(size)
            .map_err(|error| format!("open terminal pty: {error}"))?;
        let mut command = CommandBuilder::new(&spawn.program);
        for arg in &spawn.args {
            command.arg(arg);
        }
        command.cwd(&spawn.cwd);
        // The shell gets the person's own session and nothing else. The app
        // may have been opened from another terminal, or from inside an
        // agent's session, and would otherwise hand those variables on (an
        // agent started here then believes it is nested in that one). The
        // login shell builds PATH and the rest from the person's profile.
        command.env_clear();
        for (name, value) in env::vars_os() {
            if shell_may_inherit(&name.to_string_lossy()) {
                command.env(name, value);
            }
        }
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "Mycl");
        // An app opened from the Dock has no locale; a shell in the C locale
        // breaks accents, emoji and line editing.
        if env::var_os("LANG").is_none_or(|lang| lang.is_empty()) && env::var_os("LC_ALL").is_none() {
            command.env("LANG", "en_US.UTF-8");
        }
        let child = pty
            .slave
            .spawn_command(command)
            .map_err(|error| format!("spawn terminal shell: {error}"))?;
        drop(pty.slave);
        let process_group = pty
            .master
            .process_group_leader()
            .map(|pid| pid as u32)
            .or_else(|| child.process_id())
            .ok_or_else(|| "terminal shell has no process id".to_string())?;
        let reader = pty
            .master
            .try_clone_reader()
            .map_err(|error| format!("read terminal pty: {error}"))?;
        let writer = pty
            .master
            .take_writer()
            .map_err(|error| format!("write terminal pty: {error}"))?;
        let (id, generation) = {
            let mut inner = lock(&self.inner);
            inner.generation += 1;
            (Uuid::new_v4().simple().to_string(), inner.generation)
        };
        Ok((
            Arc::new(Session {
                id,
                generation,
                process_group,
                child: Mutex::new(child),
                writer: Mutex::new(writer),
                master: Mutex::new(pty.master),
                replay: Mutex::new(ReplayState {
                    bytes: Vec::new(),
                    sink: Some(sink),
                }),
                alive: AtomicBool::new(true),
                exited: AtomicBool::new(false),
            }),
            reader,
        ))
    }

    /// Read the pty on one thread and hand frames to the page on another.
    /// The returned receiver disconnects when every byte read has been sent.
    fn start_reader(session: Arc<Session>, mut reader: Box<dyn Read + Send>) -> mpsc::Receiver<()> {
        let (chunks, pending) = mpsc::sync_channel::<Vec<u8>>(PENDING_CHUNKS);
        let (drained_guard, drained) = mpsc::channel::<()>();
        thread::spawn(move || {
            let mut buffer = [0_u8; READ_CHUNK_BYTES];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if chunks.send(buffer[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
        thread::spawn(move || {
            let _drained_guard = drained_guard;
            while let Ok(mut frame) = pending.recv() {
                let deadline = Instant::now() + FRAME_WINDOW;
                while frame.len() < FRAME_BYTES {
                    let wait = deadline.saturating_duration_since(Instant::now());
                    match pending.recv_timeout(wait) {
                        Ok(more) => frame.extend_from_slice(&more),
                        Err(_) => break,
                    }
                }
                // The replay grows and the sink is read under one lock, the
                // lock `open` takes to attach: a frame is in the replay a new
                // page receives or is sent to that page live, never both.
                let sink = {
                    let mut replay = lock(&session.replay);
                    replay.bytes.extend_from_slice(&frame);
                    if replay.bytes.len() > REPLAY_BYTES {
                        let excess = replay.bytes.len() - REPLAY_BYTES;
                        replay.bytes.drain(..excess);
                    }
                    replay.sink.clone()
                };
                if let Some(sink) = sink {
                    sink(&session.id, TerminalEvent::Data, &frame);
                    thread::sleep(Duration::from_micros(
                        frame.len() as u64 * 1_000_000 / PAGE_BYTES_PER_SECOND,
                    ));
                }
            }
        });
        drained
    }

    fn start_waiter(
        session: Arc<Session>,
        inner: Weak<Mutex<ManagerInner>>,
        drained: mpsc::Receiver<()>,
    ) {
        thread::spawn(move || {
            let status = lock(&session.child).wait();
            session.exited.store(true, Ordering::SeqCst);
            // The shell's last words are still in the pty. Exit goes out
            // after them; a background job that keeps the pty open cannot
            // hold Exit back past the drain time.
            let _ = drained.recv_timeout(EXIT_DRAIN);
            let code = status.ok().map(|status| {
                if status.signal().is_some() {
                    128
                } else {
                    status.exit_code() as i32
                }
            });
            session.alive.store(false, Ordering::SeqCst);
            let sink = lock(&session.replay).sink.clone();
            if let Some(sink) = sink {
                sink(&session.id, TerminalEvent::Exit(code.unwrap_or(1)), &[]);
            }
            if let Some(inner) = inner.upgrade() {
                let mut inner = lock(&inner);
                if inner
                    .active
                    .as_ref()
                    .is_some_and(|active| active.id == session.id)
                {
                    inner.active = None;
                }
            }
        });
    }

    fn session(&self, id: &str) -> Result<Arc<Session>, String> {
        let inner = lock(&self.inner);
        let session = inner
            .active
            .as_ref()
            .filter(|session| session.id == id && session.alive.load(Ordering::SeqCst))
            .cloned()
            .ok_or_else(|| "stale session".to_string())?;
        Ok(session)
    }

    pub fn write(&self, session_id: &str, data: &str) -> Result<(), String> {
        let session = self.session(session_id)?;
        let mut writer = lock(&session.writer);
        writer
            .write_all(data.as_bytes())
            .and_then(|_| writer.flush())
            .map_err(|error| format!("write terminal: {error}"))
    }

    pub fn resize(&self, session_id: &str, cols: u16, rows: u16) -> Result<(), String> {
        let session = self.session(session_id)?;
        let master = lock(&session.master);
        master
            .resize(pty_size(cols, rows)?)
            .map_err(|error| format!("resize terminal: {error}"))
    }

    pub fn close(&self, session_id: &str) -> Result<(), String> {
        let session = {
            let mut inner = lock(&self.inner);
            if !inner
                .active
                .as_ref()
                .is_some_and(|session| session.id == session_id)
            {
                return Err("stale session".to_string());
            }
            inner.active.take().expect("checked terminal session")
        };
        Self::end(session, None);
        Ok(())
    }

    /// End a session the way a closing terminal window does: hang up, give
    /// the shell time to save its history and pass the hangup to its jobs,
    /// then kill whatever is left of its group. `wait_here` does the waiting
    /// on the calling thread (the app is quitting); otherwise on its own.
    fn end(session: Arc<Session>, wait_here: Option<Duration>) {
        session.alive.store(false, Ordering::SeqCst);
        lock(&session.replay).sink = None;
        crate::process_utils::hangup_process_group(session.process_group);
        let grace = wait_here.unwrap_or(END_GRACE);
        let finish = move || {
            let deadline = Instant::now() + grace;
            while !session.exited.load(Ordering::SeqCst) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            crate::process_utils::kill_process_group(session.process_group);
        };
        if wait_here.is_some() {
            finish();
        } else {
            thread::spawn(finish);
        }
    }

    fn end_active(&self, wait_here: Option<Duration>) {
        let session = lock(&self.inner).active.take();
        if let Some(session) = session {
            Self::end(session, wait_here);
        }
    }

    /// The app is quitting: the shell gets a short grace on this thread.
    pub fn shutdown(&self) {
        self.end_active(Some(QUIT_GRACE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Events {
        data: Vec<u8>,
        exits: Vec<i32>,
        sessions: Vec<String>,
    }

    fn manager(script: &str) -> (tempfile::TempDir, TerminalManager) {
        let profile = tempfile::tempdir().expect("terminal test profile");
        let manager = TerminalManager::with_spawn(
            profile.path().join("profile"),
            SpawnConfig {
                program: PathBuf::from("/bin/sh"),
                args: vec!["-c".to_string(), script.to_string()],
                cwd: profile.path().to_path_buf(),
            },
        );
        (profile, manager)
    }

    fn events() -> (TerminalFrameSink, Arc<Mutex<Events>>) {
        let events = Arc::new(Mutex::new(Events::default()));
        let target = events.clone();
        let sink: TerminalFrameSink = Arc::new(move |session, event, bytes| {
            let mut events = lock(&target);
            events.sessions.push(session.to_string());
            match event {
                TerminalEvent::Data => events.data.extend_from_slice(bytes),
                TerminalEvent::Exit(code) => events.exits.push(code),
            }
        });
        (sink, events)
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(Instant::now() < deadline, "terminal test timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn settings_are_persistent_and_open_refuses_while_disabled() {
        let (_profile, manager) = manager("printf marker");
        let (sink, _) = events();
        assert_eq!(
            manager.open(80, 24, sink.clone()).unwrap_err(),
            "terminal is disabled"
        );
        manager.set_enabled(true).unwrap();
        assert!(read_settings(&manager.profile_dir).unwrap());
        let opened = manager.open(80, 24, sink).unwrap();
        manager.close(&opened.session).unwrap();
        manager.set_enabled(false).unwrap();
        assert!(!read_settings(&manager.profile_dir).unwrap());
    }

    #[test]
    fn output_and_input_round_trip_and_reattach_replays_bytes() {
        let (_profile, manager) = manager("read line; printf 'start:%s' \"$line\"; sleep 10");
        manager.set_enabled(true).unwrap();
        let (sink, first_events) = events();
        let opened = manager.open(80, 24, sink).unwrap();
        manager.write(&opened.session, "hello\n").unwrap();
        wait_until(|| String::from_utf8_lossy(&lock(&first_events).data).contains("start:hello"));

        let (replacement, _) = events();
        let reattached = manager.open(100, 40, replacement).unwrap();
        assert_eq!(reattached.session, opened.session);
        assert_eq!(reattached.generation, opened.generation);
        assert!(String::from_utf8_lossy(&reattached.replay).contains("start:hello"));
        let session = manager.session(&opened.session).unwrap();
        let size = lock(&session.master).get_size().unwrap();
        assert_eq!((size.cols, size.rows), (100, 40));
        manager.close(&opened.session).unwrap();
    }

    #[test]
    fn resize_and_stale_session_ids_are_enforced() {
        let (_profile, manager) = manager("sleep 10");
        manager.set_enabled(true).unwrap();
        let (sink, _) = events();
        let opened = manager.open(80, 24, sink).unwrap();
        manager.resize(&opened.session, 132, 43).unwrap();
        let session = manager.session(&opened.session).unwrap();
        let size = lock(&session.master).get_size().unwrap();
        assert_eq!((size.cols, size.rows), (132, 43));
        assert_eq!(
            manager.resize("stale", 80, 24).unwrap_err(),
            "stale session"
        );
        assert_eq!(manager.write("stale", "no").unwrap_err(), "stale session");
        manager.close(&opened.session).unwrap();
        assert_eq!(
            manager.write(&opened.session, "no").unwrap_err(),
            "stale session"
        );
        assert_eq!(manager.close(&opened.session).unwrap_err(), "stale session");
    }

    #[test]
    fn large_output_is_drained_and_replay_is_bounded() {
        let script = "i=0; while [ $i -lt 5000 ]; do printf '0123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890123456789\\n'; i=$((i+1)); done; sleep 10";
        let (_profile, manager) = manager(script);
        manager.set_enabled(true).unwrap();
        let (sink, first_events) = events();
        let opened = manager.open(80, 24, sink).unwrap();
        wait_until(|| lock(&first_events).data.len() > REPLAY_BYTES);
        let (replacement, _) = events();
        let reattached = manager.open(80, 24, replacement).unwrap();
        assert_eq!(reattached.replay.len(), REPLAY_BYTES);
        manager.close(&opened.session).unwrap();
    }

    #[test]
    fn exit_is_emitted_once_and_reopen_uses_a_new_generation() {
        let (_profile, manager) = manager("printf marker; exit 7");
        manager.set_enabled(true).unwrap();
        let (sink, first_events) = events();
        let opened = manager.open(80, 24, sink).unwrap();
        wait_until(|| !lock(&first_events).exits.is_empty());
        assert_eq!(lock(&first_events).exits, vec![7]);
        let (replacement, _) = events();
        let reopened = manager.open(80, 24, replacement).unwrap();
        assert_ne!(reopened.session, opened.session);
        assert!(reopened.generation > opened.generation);
        manager.close(&reopened.session).ok();
        manager.shutdown();
    }

    #[test]
    fn a_damaged_settings_file_means_off_and_the_manager_still_starts() {
        let profile = tempfile::tempdir().unwrap();
        fs::write(settings_path(profile.path()), b"{ not json").unwrap();
        let manager = TerminalManager::new(profile.path().to_path_buf()).unwrap();
        assert!(!manager.enabled());
        fs::write(
            settings_path(profile.path()),
            br#"{"schemaVersion":99,"enabled":true}"#,
        )
        .unwrap();
        assert!(!TerminalManager::new(profile.path().to_path_buf())
            .unwrap()
            .enabled());
    }

    #[test]
    fn the_last_output_arrives_before_exit() {
        for _ in 0..20 {
            let (_profile, manager) = manager("printf 'last words'; exit 3");
            manager.set_enabled(true).unwrap();
            let order = Arc::new(Mutex::new(Vec::<String>::new()));
            let target = order.clone();
            let sink: TerminalFrameSink = Arc::new(move |_, event, bytes| {
                lock(&target).push(match event {
                    TerminalEvent::Data => String::from_utf8_lossy(bytes).to_string(),
                    TerminalEvent::Exit(code) => format!("<exit {code}>"),
                });
            });
            manager.open(80, 24, sink).unwrap();
            wait_until(|| lock(&order).iter().any(|entry| entry.starts_with("<exit")));
            let order = lock(&order);
            assert_eq!(order.last().map(String::as_str), Some("<exit 3>"));
            assert_eq!(order[..order.len() - 1].concat(), "last words");
        }
    }

    #[test]
    fn ending_a_session_hangs_up_first_so_the_shell_can_finish() {
        let profile = tempfile::tempdir().unwrap();
        let note = profile.path().join("hung-up");
        let script = format!(
            "trap 'echo saved > {}; exit 0' HUP; printf ready; while :; do sleep 0.05; done",
            note.display()
        );
        let manager = TerminalManager::with_spawn(
            profile.path().join("profile"),
            SpawnConfig {
                program: PathBuf::from("/bin/sh"),
                args: vec!["-c".to_string(), script],
                cwd: profile.path().to_path_buf(),
            },
        );
        manager.set_enabled(true).unwrap();
        let (sink, seen) = events();
        let opened = manager.open(80, 24, sink).unwrap();
        wait_until(|| String::from_utf8_lossy(&lock(&seen).data).contains("ready"));
        manager.close(&opened.session).unwrap();
        wait_until(|| note.exists());
    }

    #[test]
    fn a_shell_that_ignores_the_hangup_is_killed_after_the_grace() {
        let (_profile, manager) = manager("trap '' HUP; printf ready; sleep 60");
        manager.set_enabled(true).unwrap();
        let (sink, seen) = events();
        let opened = manager.open(80, 24, sink).unwrap();
        wait_until(|| String::from_utf8_lossy(&lock(&seen).data).contains("ready"));
        let session = manager.session(&opened.session).unwrap();
        manager.shutdown();
        wait_until(|| session.exited.load(Ordering::SeqCst));
    }

    #[test]
    fn no_garden_variable_reaches_the_shell() {
        env::set_var("GARDEN_TERMINAL_TEST_SECRET", "leak");
        env::set_var("CLAUDECODE", "1");
        let (_profile, manager) = manager("env; printf '<done>'; sleep 5");
        manager.set_enabled(true).unwrap();
        let (sink, seen) = events();
        let opened = manager.open(80, 24, sink).unwrap();
        wait_until(|| String::from_utf8_lossy(&lock(&seen).data).contains("<done>"));
        env::remove_var("GARDEN_TERMINAL_TEST_SECRET");
        env::remove_var("CLAUDECODE");
        assert!(shell_may_inherit("HOME") && shell_may_inherit("LC_ALL") && shell_may_inherit("XDG_RUNTIME_DIR"));
        assert!(!shell_may_inherit("CLAUDE_CODE_ENTRYPOINT") && !shell_may_inherit("GARDEN_PROFILE_DIR"));
        let output = String::from_utf8_lossy(&lock(&seen).data).to_string();
        assert!(!output.contains("GARDEN_"), "{output}");
        assert!(!output.contains("CLAUDECODE"), "{output}");
        assert!(output.contains("HOME="), "{output}");
        assert!(output.contains("TERM=xterm-256color"));
        assert!(output.contains("LANG=") || output.contains("LC_ALL="), "{output}");
        assert!(shell_may_inherit("HTTPS_PROXY") && shell_may_inherit("no_proxy"));
        manager.close(&opened.session).unwrap();
    }

    #[test]
    fn a_flood_reaches_the_page_no_faster_than_the_cap() {
        let (_profile, manager) = manager("yes 0123456789012345678901234567890123456789");
        manager.set_enabled(true).unwrap();
        let (sink, seen) = events();
        let opened = manager.open(80, 24, sink).unwrap();
        thread::sleep(Duration::from_millis(500));
        let received = lock(&seen).data.len() as u64;
        manager.close(&opened.session).unwrap();
        assert!(received > 0);
        // Half a second at the cap, plus one frame of slack.
        assert!(
            received <= PAGE_BYTES_PER_SECOND / 2 + 2 * FRAME_BYTES as u64,
            "{received} bytes in 500 ms"
        );
    }

    #[test]
    fn a_burst_of_small_writes_reaches_the_page_in_few_frames() {
        let script = "i=0; while [ $i -lt 2000 ]; do printf 'x'; i=$((i+1)); done; printf '<done>'; sleep 5";
        let (_profile, manager) = manager(script);
        manager.set_enabled(true).unwrap();
        let frames = Arc::new(Mutex::new((0_usize, Vec::<u8>::new())));
        let target = frames.clone();
        let sink: TerminalFrameSink = Arc::new(move |_, event, bytes| {
            if event == TerminalEvent::Data {
                let mut frames = lock(&target);
                frames.0 += 1;
                frames.1.extend_from_slice(bytes);
            }
        });
        let opened = manager.open(80, 24, sink).unwrap();
        wait_until(|| String::from_utf8_lossy(&lock(&frames).1).contains("<done>"));
        let frames = lock(&frames);
        assert_eq!(frames.1.len(), 2000 + "<done>".len());
        assert!(frames.0 < 500, "{} frames for 2006 bytes", frames.0);
        drop(frames);
        manager.close(&opened.session).unwrap();
    }

    #[test]
    fn disable_and_shutdown_kill_the_live_session() {
        for disable in [true, false] {
            let (_profile, manager) = manager("printf ready; sleep 30");
            manager.set_enabled(true).unwrap();
            let (sink, _) = events();
            let opened = manager.open(80, 24, sink).unwrap();
            if disable {
                manager.set_enabled(false).unwrap();
                assert_eq!(
                    manager.open(80, 24, events().0).unwrap_err(),
                    "terminal is disabled"
                );
            } else {
                manager.shutdown();
            }
            assert_eq!(
                manager.write(&opened.session, "no").unwrap_err(),
                "stale session"
            );
            assert!(lock(&manager.inner).active.is_none());
        }
    }
}
