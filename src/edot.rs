use crate::{
    id_vec::{Id, IdVec},
    location::{Column, Line, Movement, MovementError, Position, Selection},
    terminal::{Point, Rect},
    Error, Result,
};
use anyhow::{format_err, Context as _};
use crossbeam_channel::{select, unbounded, Receiver, Sender};
use fehler::throws;
use log::{error, info, trace};
use ropey::Rope;
use shlex::split as shlex;
use signal_hook::{iterator::Signals, SIGWINCH};
use std::{
    collections::{HashMap, VecDeque},
    fmt::Debug,
    fs::File,
    io::{self, Write},
    mem::take,
    os::raw::c_int,
    path::PathBuf,
    thread,
};
use termion::{
    clear, color, cursor,
    event::{Event, Key},
    get_tty,
    input::TermRead,
    raw::{IntoRawMode, RawTerminal},
    screen, style, terminal_size,
};

#[macro_export]
macro_rules! id {
    ($T:ident) => {
        #[derive(Debug, Copy, Clone, Eq, PartialEq)]
        pub struct $T(usize);

        impl Id for $T {
            fn id(self) -> usize {
                self.0
            }
        }
    };
}

pub struct Edot {
    signal: Receiver<c_int>,
    input: Receiver<io::Result<Event>>,
    exit: (Sender<()>, Receiver<()>),
    windows: IdVec<WindowId, Window>,
    buffers: IdVec<BufferId, Buffer>,
    commands: HashMap<String, CommandDesc>,
    output: RawTerminal<File>,
    focused: WindowId,
    tabline_dirty: bool,
    editor_dirty: bool,
    statusline_dirty: bool,
    message: Option<(Importance, String)>,
}

id!(WindowId);
id!(BufferId);

impl Edot {
    #[throws]
    pub fn new() -> Self {
        let (signals, signal) = unbounded();
        let (inputs, input) = unbounded();
        let signal_iter = Signals::new(&[SIGWINCH])?;
        thread::spawn(move || {
            for signal in signal_iter.forever() {
                signals.send(signal).unwrap();
            }
        });
        let tty = get_tty()?;
        thread::spawn(move || {
            for event in tty.events() {
                inputs.send(event).unwrap();
            }
        });
        Self {
            signal,
            input,
            exit: unbounded(),
            windows: vec![Window {
                buffer: BufferId(0),
                mode: Mode::Normal,
                selections: vec![Selection {
                    start: Position {
                        line: Line::from_one_based(1),
                        column: Column::from_one_based(1),
                    },
                    end: Position {
                        line: Line::from_one_based(1),
                        column: Column::from_one_based(1),
                    },
                }]
                .into(),
                command: String::new(),
                top: Line::from_one_based(1),
            }]
            .into(),
            buffers: vec![Buffer {
                content: Rope::from("\n"),
                name: String::from("scratch"),
                history: VecDeque::new(),
                path: None,
            }]
            .into(),
            commands: HashMap::new(),
            output: get_tty()?.into_raw_mode()?,
            focused: WindowId(0),
            tabline_dirty: true,
            editor_dirty: true,
            statusline_dirty: true,
            message: None,
        }
    }

    #[throws]
    #[allow(unreachable_code)]
    pub fn run(mut self) {
        write!(
            self.output,
            "{}{}{}",
            screen::ToAlternateScreen,
            cursor::Hide,
            cursor::SteadyBar
        )?;
        self.register::<Quit>("q")
            .register::<Quit>("quit")
            .register::<Edit>("e")
            .register::<Edit>("edit");
        loop {
            self.draw()?;
            match self.main() {
                Ok(true) => continue,
                Ok(false) => return,
                Err(err) => {
                    error!("{}", err);
                    self.show_message(Importance::Error, err.to_string());
                }
            }
        }
    }

    #[throws]
    fn main(&mut self) -> bool {
        select! {
            recv(self.input) -> input => self.event(input??)?,
            recv(self.signal) -> signal => self.signal(signal?)?,
            recv(self.exit.1) -> exit => { exit?; return Ok(false); },
        }
        true
    }

    #[throws]
    fn cmd(&mut self, args: &[&str]) {
        let name = args.get(0).context("no command given")?;
        let cmd = self
            .commands
            .get(*name)
            .ok_or_else(|| format_err!("command '{}' doesn't exist", name))?;
        (cmd.run)(
            Context {
                window: self.focused,
                editor: self,
            },
            &args[1..],
        )?;
    }

    fn register<T: Command>(&mut self, s: &str) -> &mut Self {
        self.commands.insert(s.to_owned(), CommandDesc::of::<T>());
        self
    }

    #[throws]
    fn event(&mut self, event: Event) {
        trace!("event: {:?}", event);
        match self.windows[self.focused].mode {
            Mode::Normal => match event {
                Event::Key(Key::Char('i')) => {
                    self.order_selections(self.focused);
                    self.set_mode(self.focused, Mode::Insert);
                }
                Event::Key(Key::Char('c')) => {
                    self.delete_selections(self.focused);
                    self.set_mode(self.focused, Mode::Insert);
                }
                Event::Key(Key::Char('a')) => {
                    self.order_selections(self.focused);
                    self.set_mode(self.focused, Mode::Append);
                }
                Event::Key(Key::Char('A')) => {
                    self.move_selections(self.focused, Movement::LineEnd, false)?;
                    self.set_mode(self.focused, Mode::Insert);
                }
                Event::Key(Key::Char('o')) => {
                    for selection_id in self.selections(self.focused) {
                        self.move_selection(self.focused, selection_id, Movement::LineEnd, false)?;
                        self.insert_char_after(self.focused, selection_id, '\n');
                        self.move_selection(self.focused, selection_id, Movement::Down, false)?;
                        self.move_selection(
                            self.focused,
                            selection_id,
                            Movement::LineStart,
                            false,
                        )?;
                    }
                    self.set_mode(self.focused, Mode::Insert);
                }
                Event::Key(Key::Char('x')) => {
                    // self.move_selections(self.focused, Movement::Line, false)?;
                }
                Event::Key(Key::Char('X')) => {
                    // self.move_selections(self.focused, Movement::Line, true)?;
                }
                Event::Key(Key::Char('g')) => {
                    self.set_mode(self.focused, Mode::Goto { drag: false });
                }
                Event::Key(Key::Char('G')) => {
                    self.set_mode(self.focused, Mode::Goto { drag: true });
                }
                Event::Key(Key::Char(':')) => {
                    self.set_mode(self.focused, Mode::Command);
                }
                Event::Key(Key::Char('h')) | Event::Key(Key::Left) => {
                    self.move_selections(self.focused, Movement::Left, false)?;
                }
                Event::Key(Key::Char('j')) | Event::Key(Key::Down) => {
                    self.move_selections(self.focused, Movement::Down, false)?;
                }
                Event::Key(Key::Char('k')) | Event::Key(Key::Up) => {
                    self.move_selections(self.focused, Movement::Up, false)?;
                }
                Event::Key(Key::Char('l')) | Event::Key(Key::Right) => {
                    self.move_selections(self.focused, Movement::Right, false)?;
                }
                Event::Key(Key::Char('H')) => {
                    self.move_selections(self.focused, Movement::Left, true)?;
                }
                Event::Key(Key::Char('J')) => {
                    self.move_selections(self.focused, Movement::Down, true)?;
                }
                Event::Key(Key::Char('K')) => {
                    self.move_selections(self.focused, Movement::Up, true)?;
                }
                Event::Key(Key::Char('L')) => {
                    self.move_selections(self.focused, Movement::Right, true)?;
                }
                Event::Key(Key::Char('d')) => {
                    self.delete_selections(self.focused);
                }
                _ => {}
            },
            Mode::Goto { drag } => {
                match event {
                    Event::Key(Key::Char('h')) => {
                        self.move_selections(self.focused, Movement::LineStart, drag)?;
                    }
                    Event::Key(Key::Char('j')) => {
                        self.move_selections(self.focused, Movement::FileEnd, drag)?;
                    }
                    Event::Key(Key::Char('k')) => {
                        self.move_selections(self.focused, Movement::FileStart, drag)?;
                    }
                    Event::Key(Key::Char('l')) => {
                        self.move_selections(self.focused, Movement::LineEnd, drag)?;
                    }
                    _ => {}
                };
                self.set_mode(self.focused, Mode::Normal);
            }
            mode @ Mode::Insert | mode @ Mode::Append => match event {
                Event::Key(Key::Esc) => self.set_mode(self.focused, Mode::Normal),
                Event::Key(Key::Char(c)) => {
                    for selection_id in self.selections(self.focused) {
                        match mode {
                            Mode::Insert => {
                                self.insert_char_before(self.focused, selection_id, c);
                                self.shift_selection(self.focused, selection_id, Movement::Right)?;
                            }
                            Mode::Append => {
                                self.move_selection(
                                    self.focused,
                                    selection_id,
                                    Movement::Right,
                                    true,
                                )?;
                                self.insert_char_after(self.focused, selection_id, c);
                            }
                            _ => unreachable!(),
                        }
                    }
                }
                Event::Key(Key::Backspace) => {
                    self.move_selections(self.focused, Movement::Left, false)?;
                    self.delete_selections(self.focused);
                }
                _ => {}
            },
            Mode::Command => match event {
                Event::Key(Key::Esc) => {
                    self.windows[self.focused].command.clear();
                    self.set_mode(self.focused, Mode::Normal);
                }
                Event::Key(Key::Char('\t')) => {}
                Event::Key(Key::Char('\n')) => {
                    let command = take(&mut self.windows[self.focused].command);
                    self.set_mode(self.focused, Mode::Normal);
                    let command = shlex(&command)
                        .ok_or_else(|| format_err!("failed to parse command '{}'", command))?;
                    trace!("command: {:?}", command);
                    let command = command.iter().map(|x| &**x).collect::<Vec<&str>>();
                    self.cmd(&command)?;
                }
                Event::Key(Key::Char(c)) => {
                    self.windows[self.focused].command.push(c);
                }
                Event::Key(Key::Backspace) => {
                    if self.windows[self.focused].command.pop().is_none() {
                        self.set_mode(self.focused, Mode::Normal);
                    } else {
                    }
                }
                _ => {}
            },
        }
    }

    #[throws]
    fn signal(&mut self, signal: c_int) {
        info!("received signal: {}", signal);
        match signal {
            signal_hook::SIGWINCH => self.draw()?,
            _ => {}
        }
    }

    #[throws]
    fn draw(&mut self) {
        let (width, height) = terminal_size()?;

        let region = Rect {
            start: Point { x: 1, y: 1 },
            end: Point { x: width, y: 1 },
        };
        self.draw_tabs(region)?;

        let region = Rect {
            start: Point { x: 1, y: 2 },
            end: Point {
                x: width,
                y: height - 1,
            },
        };
        self.draw_window(self.focused, region)?;

        let region = Rect {
            start: Point { x: 1, y: height },
            end: Point {
                x: width,
                y: height,
            },
        };
        self.draw_status(region)?;

        self.output.flush()?;
    }

    #[throws]
    fn draw_tabs(&mut self, region: Rect) {
        write!(self.output, "{}{}", region.start.goto(), clear::CurrentLine)?;
        for window_id in (0..self.windows.len()).map(WindowId) {
            let window = &self.windows[window_id];
            let buffer = &self.buffers[window.buffer];
            write!(self.output, "{} ", buffer.name)?;
        }
        self.tabline_dirty = false;
    }

    #[throws]
    fn draw_status(&mut self, region: Rect) {
        if let Some((_importance, message)) = self.message.take() {
            write!(
                self.output,
                "{}{}{}{} {} {}",
                region.start.goto(),
                clear::CurrentLine,
                color::Bg(color::Red),
                color::Fg(color::White),
                message,
                style::Reset,
            )?;
        } else {
            let mode = self.windows[self.focused].mode;
            write!(
                self.output,
                "{}{}{} {:?} {}",
                region.start.goto(),
                clear::CurrentLine,
                style::Invert,
                mode,
                style::Reset,
            )?;
            match mode {
                Mode::Command => {
                    write!(
                        self.output,
                        " :{}{} {}",
                        self.windows[self.focused].command,
                        style::Invert,
                        style::Reset,
                    )?;
                }
                _ => {}
            }
            self.statusline_dirty = false;
        }
    }

    #[throws]
    fn draw_window(&mut self, window_id: WindowId, region: Rect) {
        // TODO: draw a block where the next character will go in insert mode
        let window = &self.windows[window_id];
        let buffer = &self.buffers[window.buffer];
        let mut lines = buffer.content.lines_at(window.top.zero_based()).enumerate();
        let mut range_y = region.range_y();
        'outer: while let Some(y) = range_y.next() {
            write!(self.output, "{}{}", cursor::Goto(1, y), clear::CurrentLine)?;
            if let Some((line, text)) = lines.next() {
                let mut chars = text.chars().enumerate();
                let mut col = 0;
                while let Some((file_col, mut c)) = chars.next() {
                    if col == region.width() as usize + 1 {
                        write!(self.output, "\r\n{}", clear::CurrentLine)?;
                        if range_y.next().is_none() {
                            break 'outer;
                        }
                        col = 0;
                    }
                    let pos = Position {
                        line: Line::from_zero_based(line),
                        column: Column::from_zero_based(file_col),
                    };
                    if c == '\n' {
                        c = '␤';
                    }
                    // TODO: special case tab rendering
                    if window
                        .selections
                        .iter()
                        .map(|s| s.valid(&buffer.content))
                        .any(|s| s.contains(pos))
                    {
                        write!(self.output, "{}{}{}", style::Invert, c, style::Reset)?;
                    } else {
                        write!(self.output, "{}", c)?;
                    }
                    col += 1;
                }
            }
        }
    }

    pub fn show_message(&mut self, importance: Importance, message: String) {
        self.message = Some((importance, message));
    }

    pub fn quit(&mut self) {
        self.exit.0.send(()).unwrap();
    }

    pub fn set_mode(&mut self, window: WindowId, mode: Mode) {
        self.windows[window].mode = mode;
        match mode {
            Mode::Normal => {}
            Mode::Insert => {}
            Mode::Append => {}
            Mode::Goto { .. } => {}
            Mode::Command => {}
        }
    }

    pub fn selections(&self, window: WindowId) -> impl Iterator<Item = SelectionId> {
        let window = &self.windows[window];
        (0..window.selections.len()).map(SelectionId)
    }

    pub fn insert_char_before(&mut self, window_id: WindowId, selection_id: SelectionId, c: char) {
        let window = &mut self.windows[window_id];
        let buffer = &mut self.buffers[window.buffer];
        let selection = &mut window.selections[selection_id];
        selection.start.insert_char(&mut buffer.content, c);
    }

    pub fn insert_char_after(&mut self, window_id: WindowId, selection_id: SelectionId, c: char) {
        let window = &mut self.windows[window_id];
        let buffer = &mut self.buffers[window.buffer];
        let selection = &mut window.selections[selection_id];
        selection.end.insert_char(&mut buffer.content, c);
    }

    #[throws(MovementError)]
    pub fn move_selection(
        &mut self,
        window_id: WindowId,
        selection_id: SelectionId,
        movement: Movement,
        drag: bool,
    ) {
        let window = &mut self.windows[window_id];
        let buffer = &mut self.buffers[window.buffer];
        let selection = &mut window.selections[selection_id];
        selection.end.move_to(&buffer.content, movement)?;
        if !drag {
            selection.start = selection.end;
        }
    }

    #[throws(MovementError)]
    pub fn move_selections(&mut self, window_id: WindowId, movement: Movement, drag: bool) {
        for selection_id in self.selections(window_id) {
            self.move_selection(window_id, selection_id, movement, drag)?;
        }
    }

    #[throws(MovementError)]
    pub fn shift_selection(
        &mut self,
        window_id: WindowId,
        selection_id: SelectionId,
        movement: Movement,
    ) {
        let window = &mut self.windows[window_id];
        let buffer = &mut self.buffers[window.buffer];
        let selection = &mut window.selections[selection_id];
        selection.start.move_to(&buffer.content, movement)?;
        selection.end.move_to(&buffer.content, movement)?;
    }

    #[throws(MovementError)]
    pub fn shift_selections(&mut self, window_id: WindowId, movement: Movement) {
        for selection_id in self.selections(window_id) {
            self.shift_selection(window_id, selection_id, movement)?;
        }
    }

    pub fn delete_selection(&mut self, window_id: WindowId, selection_id: SelectionId) {
        let window = &mut self.windows[window_id];
        let buffer = &mut self.buffers[window.buffer];
        let selection = &mut window.selections[selection_id];
        selection.remove_from(&mut buffer.content);
    }

    pub fn delete_selections(&mut self, window_id: WindowId) {
        for selection_id in self.selections(window_id) {
            self.delete_selection(window_id, selection_id);
        }
    }

    pub fn flip_selection(&mut self, window_id: WindowId, selection_id: SelectionId) {
        let window = &mut self.windows[window_id];
        let selection = &mut window.selections[selection_id];
        selection.flip();
    }

    pub fn order_selections(&mut self, window_id: WindowId) {
        for selection_id in self.selections(window_id) {
            self.order_selection(window_id, selection_id);
        }
    }

    pub fn order_selection(&mut self, window_id: WindowId, selection_id: SelectionId) {
        let window = &mut self.windows[window_id];
        let selection = &mut window.selections[selection_id];
        selection.order();
    }

    pub fn flip_selections(&mut self, window_id: WindowId) {
        for selection_id in self.selections(window_id) {
            self.flip_selection(window_id, selection_id);
        }
    }

    pub fn for_each_selection<F>(&mut self, window_id: WindowId, mut f: F) -> Result<(), Error>
    where
        F: FnMut(&mut Self, WindowId, SelectionId) -> Result<(), Error>,
    {
        let mut errors = Vec::new();
        for selection_id in self.selections(window_id) {
            if let Err(e) = f(self, window_id, selection_id) {
                errors.push(e);
            }
        }
        errors.pop().map_or(Ok(()), Err)
    }
}

impl Drop for Edot {
    fn drop(&mut self) {
        let _ = write!(
            self.output,
            "{}{}{}",
            cursor::Show,
            cursor::SteadyBlock,
            screen::ToMainScreen
        );
    }
}

pub struct Window {
    buffer: BufferId,
    mode: Mode,
    selections: IdVec<SelectionId, Selection>,
    command: String,
    top: Line,
}

id!(SelectionId);

pub struct Buffer {
    path: Option<PathBuf>,
    name: String,
    content: Rope,
    history: VecDeque<Modification>,
}

#[derive(Debug, Copy, Clone)]
pub enum Modification {}

#[derive(Debug, Copy, Clone)]
pub enum Mode {
    Normal,
    Insert,
    Append,
    Goto { drag: bool },
    Command,
}

#[derive(Debug, Copy, Clone)]
pub enum Importance {
    Error,
}

pub struct Context<'a> {
    editor: &'a mut Edot,
    window: WindowId,
}

pub trait Command: Sized {
    const DESCRIPTION: &'static str;
    const REQUIRED_ARGUMENTS: usize = 0;

    fn run(cx: Context, args: &[&str]) -> Result;
}

pub struct CommandDesc {
    description: &'static str,
    required_arguments: usize,
    run: fn(cx: Context, args: &[&str]) -> Result,
}

impl CommandDesc {
    fn of<T: Command>() -> Self {
        Self {
            description: T::DESCRIPTION,
            required_arguments: T::REQUIRED_ARGUMENTS,
            run: T::run,
        }
    }
}

enum Quit {}

impl Command for Quit {
    const DESCRIPTION: &'static str = "quits the editor";

    #[throws]
    fn run(cx: Context, _args: &[&str]) {
        cx.editor.quit();
    }
}

enum Edit {}

impl Command for Edit {
    const DESCRIPTION: &'static str = "open a file";
    const REQUIRED_ARGUMENTS: usize = 1;

    #[throws]
    fn run(cx: Context, args: &[&str]) {
        let name = String::from(args[0]);
        let path = PathBuf::from(&name).canonicalize()?;
        let reader = File::open(&path)?;
        let buffer = Buffer {
            path: Some(path),
            name,
            content: Rope::from_reader(reader)?,
            history: VecDeque::new(),
        };
        let buffer_id = BufferId(cx.editor.buffers.len());
        cx.editor.buffers.push(buffer);
        let window = Window {
            buffer: buffer_id,
            command: String::new(),
            mode: Mode::Normal,
            selections: vec![Selection {
                // TODO move this out
                start: Position {
                    line: Line::from_one_based(1),
                    column: Column::from_one_based(1),
                },
                end: Position {
                    line: Line::from_one_based(1),
                    column: Column::from_one_based(1),
                },
            }]
            .into(),
            top: Line::from_one_based(1),
        };
        let window_id = WindowId(cx.editor.windows.len());
        cx.editor.windows.push(window);
        cx.editor.focused = window_id;
    }
}
