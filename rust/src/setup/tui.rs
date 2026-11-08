//! Native Ratatui/Crossterm setup. Every request and discovery session is owned
//! and joined before terminal restoration; no task may edit the draft directly.
use super::{
    SetupError,
    cli::api_for,
    state::{
        Action, CREDENTIALS_URL, FoundDevice, Request, Screen, SetupEffect, SetupEvent, SetupState,
        reduce,
    },
    widgets::{Edit, Surface, render},
};
use crate::{
    config::{ConfigDocument, ConfigStore, SaveOutcome},
    discovery::{Discovery, DiscoveryEvent, DiscoveryMode},
    http::{Endpoints, HttpClients},
};
use crossterm::{
    cursor::{Hide, Show},
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    collections::VecDeque,
    io::{self, IsTerminal, Write},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        // Construct the guard before any fallible terminal command.
        let guard = Self;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            Hide
        )?;
        Ok(guard)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            Show
        );
        let _ = io::stdout().flush();
    }
}
struct Job {
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}
#[derive(Default)]
struct Jobs {
    pair: Option<Job>,
    search: Option<Job>,
    discovery: Option<Job>,
}
impl Jobs {
    fn slot(&mut self, request: Request) -> &mut Option<Job> {
        match request {
            Request::Pair => &mut self.pair,
            Request::Search => &mut self.search,
            Request::Discovery => &mut self.discovery,
        }
    }
    async fn stop(&mut self, request: Request) {
        if let Some(job) = self.slot(request).take() {
            job.cancel.cancel();
            let _ = job.handle.await;
        }
    }
    async fn stop_all(&mut self) {
        // Signal every worker before joining any, including discovery's sockets.
        for job in [&self.pair, &self.search, &self.discovery]
            .into_iter()
            .flatten()
        {
            job.cancel.cancel();
        }
        self.stop(Request::Pair).await;
        self.stop(Request::Search).await;
        self.stop(Request::Discovery).await;
    }
}
impl Drop for Jobs {
    fn drop(&mut self) {
        for job in [&self.pair, &self.search, &self.discovery]
            .into_iter()
            .flatten()
        {
            job.cancel.cancel();
        }
    }
}

/// Constructor endpoints are a test seam, never a production endpoint override.
pub async fn run(
    store: &ConfigStore,
    document: ConfigDocument,
    endpoints: Endpoints,
    shutdown: CancellationToken,
    http_tracing: bool,
) -> Result<(), SetupError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("Use setup-cli in a non-interactive terminal");
        return Err(SetupError::Io(io::ErrorKind::Unsupported));
    }
    let guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut events = EventStream::new();
    let (sender, mut receiver) = mpsc::channel(64);
    let mut state = SetupState::new(document);
    let mut jobs = Jobs::default();
    let mut browser_failed = false;
    let result: Result<(), SetupError> = async {
        loop {
            let mut surface = Surface::default();
            terminal.draw(|frame| surface = render(frame, &mut state))?;
            let event = tokio::select! {
                biased;
                _ = shutdown.cancelled() => return Err(SetupError::Interrupted),
                event = receiver.recv() => match event { Some(event) => event, None => return Err(SetupError::Io(io::ErrorKind::BrokenPipe)) },
                input = events.next() => match input {
                    Some(Ok(event)) => match translate(event, &state, &surface) { Some(event) => event, None => continue },
                    Some(Err(error)) => return Err(error.into()),
                    None => return Err(SetupError::Interrupted),
                },
            };
            let mut effects: VecDeque<_> = reduce(&mut state, event).into();
            while let Some(effect) = effects.pop_front() {
                match effect {
                    SetupEffect::Cancel(request) => jobs.stop(request).await,
                    SetupEffect::CancelAll => jobs.stop_all().await,
                    SetupEffect::Exit => return Ok(()),
                    SetupEffect::Save { exit, migration } => {
                        if shutdown.is_cancelled() { return Err(SetupError::Interrupted); }
                        let result = store.save(&state.draft).map_err(|error| error.to_string());
                        effects.extend(reduce(&mut state, SetupEvent::Saved { result, exit, migration }));
                    }
                    SetupEffect::OpenCredentials => {
                        if webbrowser::open(CREDENTIALS_URL).is_err() {
                            browser_failed = true;
                            state.message = format!("Browser failed; open manually: {CREDENTIALS_URL}");
                        } else { state.message = "Credentials page opened in your browser".into(); }
                    }
                    SetupEffect::Pair { stamp, code } => {
                        jobs.stop(Request::Pair).await;
                        match api_for(&state.draft, endpoints.clone(), http_tracing) {
                            Err(error) => { effects.extend(reduce(&mut state, SetupEvent::PairResult(stamp, Err(error.to_string())))); }
                            Ok(api) => {
                                let cancel = CancellationToken::new(); let child = cancel.clone(); let sender = sender.clone();
                                let handle = tokio::spawn(async move {
                                    let result = tokio::select! { biased; _ = child.cancelled() => return, result = api.pair_with_code(&code) => result.map_err(|error| error.to_string()) };
                                    tokio::select! { biased; _ = child.cancelled() => {}, _ = sender.send(SetupEvent::PairResult(stamp, result)) => {} }
                                });
                                jobs.pair = Some(Job { cancel, handle });
                            }
                        }
                    }
                    SetupEffect::Search { stamp, query } => {
                        jobs.stop(Request::Search).await;
                        match api_for(&state.draft, endpoints.clone(), http_tracing) {
                            Err(error) => { effects.extend(reduce(&mut state, SetupEvent::SearchResult(stamp, Err(error.to_string())))); }
                            Ok(api) => {
                                let cancel = CancellationToken::new(); let child = cancel.clone(); let sender = sender.clone();
                                let handle = tokio::spawn(async move {
                                    let result = tokio::select! { biased; _ = child.cancelled() => return, result = api.search_channels(&query) => result.map_err(|error| error.to_string()) };
                                    tokio::select! { biased; _ = child.cancelled() => {}, _ = sender.send(SetupEvent::SearchResult(stamp, result)) => {} }
                                });
                                jobs.search = Some(Job { cancel, handle });
                            }
                        }
                    }
                    SetupEffect::Discover { stamp } => {
                        jobs.stop(Request::Discovery).await;
                        let discovery = api_for(&state.draft, endpoints.clone(), http_tracing).and_then(|api| {
                            let http = HttpClients::new(state.boolean("use_proxy"), http_tracing).map_err(crate::api::ApiError::from)?;
                            Ok(Discovery::new(api, http))
                        });
                        match discovery {
                            Err(error) => {
                                effects.extend(reduce(&mut state, SetupEvent::DiscoveryWarning(stamp, error.to_string())));
                                effects.extend(reduce(&mut state, SetupEvent::DiscoveryComplete(stamp)));
                            }
                            Ok(discovery) => {
                                let cancel = CancellationToken::new(); let child = cancel.clone(); let sender = sender.clone();
                                let handle = tokio::spawn(async move {
                                    let mut session = discovery.start(DiscoveryMode::ActiveOnce);
                                    loop {
                                        let event = tokio::select! { biased; _ = child.cancelled() => break, event = session.next() => event };
                                        let (event, complete) = match event {
                                            Some(DiscoveryEvent::Device(device)) => (SetupEvent::DeviceFound(stamp, FoundDevice { screen_id: device.screen_id, name: device.name, selected: false }), false),
                                            Some(DiscoveryEvent::BackendWarning { backend, message }) => (SetupEvent::DiscoveryWarning(stamp, format!("{backend}: {message}")), false),
                                            Some(DiscoveryEvent::Complete) | None => (SetupEvent::DiscoveryComplete(stamp), true),
                                        };
                                        let sent = tokio::select! { biased; _ = child.cancelled() => false, result = sender.send(event) => result.is_ok() };
                                        if complete || !sent { break; }
                                    }
                                    session.stop().await;
                                });
                                jobs.discovery = Some(Job { cancel, handle });
                            }
                        }
                    }
                }
            }
        }
    }.await;
    jobs.stop_all().await;
    drop(events);
    drop(terminal);
    drop(guard);
    if browser_failed {
        println!("Open the credentials page manually: {CREDENTIALS_URL}");
    }
    let path = store.config_file().display().to_string();
    let path: String = path
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    match state.last_save {
        Some(SaveOutcome::Saved) => println!("Config saved to {path}"),
        Some(SaveOutcome::Unchanged) => println!("Config unchanged at {path}"),
        None => println!("No config file was saved to {path}"),
    }
    result?;
    if state.interrupted {
        Err(SetupError::Interrupted)
    } else {
        Ok(())
    }
}

pub fn translate(event: Event, state: &SetupState, surface: &Surface) -> Option<SetupEvent> {
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => {
            let control = key.modifiers.contains(KeyModifiers::CONTROL);
            if control && key.code == KeyCode::Char('c') {
                return Some(if state.screen == Screen::ConfirmExit {
                    SetupEvent::Activate(Action::Discard)
                } else {
                    SetupEvent::RequestExit { interrupted: true }
                });
            }
            if surface.small {
                return None;
            }
            match key.code {
                KeyCode::Tab => Some(SetupEvent::Move(1)),
                KeyCode::BackTab => Some(SetupEvent::Move(-1)),
                KeyCode::Esc => Some(SetupEvent::Escape),
                KeyCode::Up => Some(SetupEvent::Move(-1)),
                KeyCode::Down => Some(SetupEvent::Move(1)),
                KeyCode::Enter => state.focused_action().map(SetupEvent::Activate),
                KeyCode::Left | KeyCode::Right
                    if state.focused_action() == Some(Action::Slider) =>
                {
                    Some(SetupEvent::Slider(if key.code == KeyCode::Left {
                        -1
                    } else {
                        1
                    }))
                }
                KeyCode::Home if state.focused_field().is_none() => Some(SetupEvent::Focus(0)),
                KeyCode::End if state.focused_field().is_none() => {
                    Some(SetupEvent::Focus(state.rows().len().saturating_sub(1)))
                }
                KeyCode::PageUp => Some(SetupEvent::Move(-8)),
                KeyCode::PageDown => Some(SetupEvent::Move(8)),
                _ if state.focused_field().is_some() => {
                    let edit = match key.code {
                        KeyCode::Char('a') if control => Edit::SelectAll,
                        KeyCode::Left => Edit::Left,
                        KeyCode::Right => Edit::Right,
                        KeyCode::Home => Edit::Home,
                        KeyCode::End => Edit::End,
                        KeyCode::Backspace => Edit::Backspace,
                        KeyCode::Delete => Edit::Delete,
                        KeyCode::Char(character)
                            if !control && !key.modifiers.contains(KeyModifiers::ALT) =>
                        {
                            Edit::Insert(character.to_string())
                        }
                        _ => return None,
                    };
                    Some(SetupEvent::Edit(edit))
                }
                KeyCode::Char('s') => Some(SetupEvent::Activate(
                    if state.screen == Screen::ConfirmExit {
                        Action::SaveExit
                    } else if state.screen == Screen::Migration {
                        Action::Migrate
                    } else {
                        Action::Save
                    },
                )),
                KeyCode::Char('q') => Some(SetupEvent::RequestExit { interrupted: false }),
                KeyCode::Char(' ') => state.focused_action().map(SetupEvent::Activate),
                _ => None,
            }
        }
        Event::Paste(text) if !surface.small => Some(SetupEvent::Edit(Edit::Insert(text))),
        Event::Mouse(mouse) if !surface.small => match mouse.kind {
            MouseEventKind::ScrollDown => Some(SetupEvent::Move(3)),
            MouseEventKind::ScrollUp => Some(SetupEvent::Move(-3)),
            MouseEventKind::Down(MouseButton::Left) => {
                let position = ratatui::layout::Position::new(mouse.column, mouse.row);
                if let Some(hit) = surface.hits.iter().find(|hit| hit.area.contains(position)) {
                    if matches!(hit.action, Action::Input(_)) {
                        Some(SetupEvent::Focus(hit.row))
                    } else if hit.action == Action::Slider {
                        let step = u32::from(mouse.column.saturating_sub(hit.area.x)) * 20
                            / u32::from(hit.area.width.saturating_sub(1).max(1));
                        Some(SetupEvent::SliderTo(hit.row, (step * 100) as u16))
                    } else {
                        Some(SetupEvent::Click(hit.row, hit.action.clone()))
                    }
                } else if surface.modal.is_some_and(|area| !area.contains(position)) {
                    Some(SetupEvent::Escape)
                } else {
                    None
                }
            }
            _ => None,
        },
        // Returning a no-op event ensures a resize immediately redraws geometry.
        Event::Resize(_, _) => Some(SetupEvent::Focus(state.focus)),
        _ => None,
    }
}
