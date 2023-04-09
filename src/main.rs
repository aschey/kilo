use std::{
    borrow::Cow,
    env::args,
    fs::File,
    io::{self, BufReader, Read, Stdout, Write},
    mem,
    os::fd::AsRawFd,
    path::Path,
    slice,
};

use libc::{c_ushort, STDOUT_FILENO, TIOCGWINSZ};
use nix::{
    errno::Errno,
    ioctl_read_bad,
    libc::{VMIN, VTIME},
    sys::termios::{
        tcgetattr, tcsetattr, ControlFlags, InputFlags, LocalFlags, OutputFlags, SetArg, Termios,
    },
};
use ropey::Rope;

ioctl_read_bad!(read_winsize, TIOCGWINSZ, winsize);

const fn ctrl_key(b: u8) -> char {
    (b & 0b00011111) as char
}

/*** init ***/

fn main() -> io::Result<()> {
    let mut editor = Editor::new()?;
    if let Err(e) = editor.run() {
        println!("Error {e}");
    }

    editor.editor_refresh_screen()?;
    // Set terminal attributes back to the original termios state
    editor.disable_raw_mode()?;
    Ok(())
}

#[derive(Debug)]
#[repr(C)]
pub struct winsize {
    ws_row: c_ushort,
    ws_col: c_ushort,
    ws_xpixel: c_ushort,
    ws_ypixel: c_ushort,
}

enum EditorKey {
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    PageUp,
    PageDown,
    Home,
    End,
    Delete,
    Other(char),
}

struct Editor {
    orig_termios: Termios,
    stdin_fd: i32,
    stdout: Stdout,
    screen_rows: usize,
    screen_cols: usize,
    cursor_row: usize,
    cursor_col: usize,
    row_offset: usize,
    rope: Rope,
}

impl Editor {
    fn new() -> io::Result<Self> {
        let stdin_fd = io::stdin().as_raw_fd();
        let stdout = io::stdout();
        // Read current termios settings
        let termios = tcgetattr(stdin_fd)?;
        let (screen_rows, screen_cols) = Self::get_window_size()?;
        Ok(Self {
            orig_termios: termios,
            stdin_fd,
            stdout,
            screen_rows: screen_rows as usize,
            screen_cols: screen_cols as usize,
            cursor_row: 0,
            cursor_col: 0,
            row_offset: 0,
            rope: Rope::default(),
        })
    }

    fn run(&mut self) -> io::Result<()> {
        // Use a new copy of the termios instance so we can restore the original state later
        self.enable_raw_mode()?;
        let args: Vec<_> = args().collect();
        if args.len() > 1 {
            self.editor_open(Path::new(&args[1]))?;
        }

        loop {
            self.editor_refresh_screen()?;
            if !self.editor_process_keypress()? {
                return Ok(());
            }
        }
    }

    /*** file i/o ***/
    fn editor_open(&mut self, path: &Path) -> io::Result<()> {
        self.rope = Rope::from_reader(BufReader::new(File::open(path)?))?;
        Ok(())
    }

    /*** output ***/
    fn editor_scroll(&mut self) {
        // If cursor is above visible window, scroll up to where cursor is
        if self.cursor_row < self.row_offset {
            self.row_offset = self.cursor_row;
        }
        // If cursor is below visible window, scroll down to where cursor is
        if self.cursor_row >= self.row_offset + self.screen_rows {
            self.row_offset = self.cursor_row - self.screen_rows + 1;
        }
    }

    fn editor_draw_rows(&mut self) -> io::Result<()> {
        for y in 0..self.screen_rows {
            // K - erase in line (clear current line)
            self.stdout.write_all(b"\x1b[K")?;
            let file_row = y + self.row_offset;
            if self.rope.len_chars() == 0 || file_row >= self.rope.len_lines() {
                if self.rope.len_chars() == 0 && y == self.screen_rows / 3 {
                    let mut message = "Kilo editor -- version 0.0.1";
                    if message.len() > self.screen_cols {
                        message = &message[..self.screen_cols];
                    }

                    let mut padding = (self.screen_cols - message.len()) / 2;
                    if padding > 0 {
                        self.stdout.write_all(b"~")?;
                        padding -= 1;
                    }
                    write!(&mut self.stdout, "{}{message}", " ".repeat(padding))?;
                } else {
                    self.stdout.write_all(b"~")?;
                }
            } else {
                let mut line_slice = self.rope.line(file_row);
                if line_slice.len_chars() > 0 && line_slice.char(line_slice.len_chars() - 1) == '\n'
                {
                    line_slice = line_slice.slice(..line_slice.len_chars() - 1);
                }

                if line_slice.len_chars() > self.screen_cols {
                    line_slice = line_slice.slice(..self.screen_cols);
                }
                let s: Cow<str> = line_slice.into();
                self.stdout.write_all(s.as_bytes())?;
            }

            if y < self.screen_rows - 1 {
                self.stdout.write_all(b"\r\n")?;
            }
        }

        Ok(())
    }

    fn editor_refresh_screen(&mut self) -> io::Result<()> {
        self.editor_scroll();
        // escape sequence
        // \x1b (27) - escape character (mapped to ESC on keyboard)
        // [ - sequence start

        // l - reset mode
        // ?25 - cursor
        // hides the cursor
        self.stdout.write_all(b"\x1b[?25l")?;
        // H - cursor position
        // same as \x1b[1;1H - position cursor at row 1 column 1
        self.stdout.write_all(b"\x1b[H")?;

        self.editor_draw_rows()?;

        // Move cursor to the correct position
        write!(
            &mut self.stdout,
            "\x1b[{};{}H",
            self.cursor_row - self.row_offset + 1,
            self.cursor_col + 1
        )?;
        // h - set mode
        // ?25 - cursor
        // shows the cursor
        self.stdout.write_all(b"\x1b[?25h")?;

        self.stdout.flush()
    }

    /*** input ***/
    fn editor_move_cursor(&mut self, key: EditorKey) {
        match key {
            EditorKey::ArrowLeft if self.cursor_col > 0 => self.cursor_col -= 1,
            EditorKey::ArrowRight if self.cursor_col < self.screen_cols => self.cursor_col += 1,
            EditorKey::ArrowUp if self.cursor_row > 0 => self.cursor_row -= 1,
            EditorKey::ArrowDown
                if self.cursor_row < (self.rope.len_lines() - 1).max(self.screen_rows) =>
            {
                self.cursor_row += 1
            }
            EditorKey::PageUp => self.cursor_row = 0,
            EditorKey::PageDown => self.cursor_row = self.screen_rows,
            EditorKey::Home => self.cursor_col = 0,
            EditorKey::End => self.cursor_col = self.screen_cols,
            _ => {}
        }
    }

    fn editor_read_key(&self) -> io::Result<EditorKey> {
        let mut stdin_iter = io::stdin();
        loop {
            let mut b = 0;
            if stdin_iter.read(slice::from_mut(&mut b))? == 1 {
                let c = b as char;
                if c == '\x1b' {
                    let mut buf = [0u8; 3];

                    if stdin_iter.read(&mut buf[..2])? < 2 {
                        return Ok(EditorKey::Other(c));
                    }

                    return Ok(match buf[..2] {
                        [b'[', b'0'..=b'9'] => {
                            if stdin_iter.read(&mut buf[2..])? < 1 {
                                EditorKey::Other(c)
                            } else if buf[2] == b'~' {
                                match buf[1] {
                                    b'1' => EditorKey::Home,
                                    b'3' => EditorKey::Delete,
                                    b'4' => EditorKey::End,
                                    b'5' => EditorKey::PageUp,
                                    b'6' => EditorKey::PageDown,
                                    b'7' => EditorKey::Home,
                                    b'8' => EditorKey::End,
                                    _ => EditorKey::Other(c),
                                }
                            } else {
                                EditorKey::Other(c)
                            }
                        }
                        [b'[', b'A'] => EditorKey::ArrowUp,
                        [b'[', b'B'] => EditorKey::ArrowDown,
                        [b'[', b'C'] => EditorKey::ArrowRight,
                        [b'[', b'D'] => EditorKey::ArrowLeft,
                        [b'[', b'F'] => EditorKey::End,
                        [b'[', b'H'] => EditorKey::Home,
                        [b'O', b'F'] => EditorKey::End,
                        [b'O', b'H'] => EditorKey::Home,
                        _ => EditorKey::Other(c),
                    });
                } else {
                    return Ok(EditorKey::Other(c));
                }
            }
        }
    }

    fn editor_process_keypress(&mut self) -> io::Result<bool> {
        let c = self.editor_read_key()?;
        if let EditorKey::Other(c) = c {
            if c == ctrl_key(b'q') {
                return Ok(false);
            }
        }
        self.editor_move_cursor(c);
        Ok(true)
    }

    /*** terminal ***/
    fn get_window_size() -> io::Result<(u16, u16)> {
        unsafe {
            let mut size: winsize = mem::zeroed();
            read_winsize(STDOUT_FILENO, &mut size)?;
            Ok((size.ws_row, size.ws_col))
        }
    }

    fn enable_raw_mode(&self) -> io::Result<()> {
        let mut termios = self.orig_termios.clone();

        // Disable some input flags
        // BRKINT - a break condition sends a SIGINT
        // ICRNL - translating carriage returns (Ctrl-M) into newlines
        // INPCK - parity checking, doesn't really apply these days
        // ISTRIP - input stripping, causes 8th byte of each input to be stripped, probably already disabled
        // IXON - Ctrl+S (stops data transmission) and Ctrl+Q (resume data transmission)

        termios.input_flags &= !(InputFlags::BRKINT
            | InputFlags::ICRNL
            | InputFlags::INPCK
            | InputFlags::ISTRIP
            | InputFlags::IXON);

        // Disable some output flags
        // OPOST - translates \n to \r\n
        termios.output_flags &= !(OutputFlags::OPOST);

        // Disable "local flags" which is a dumping ground for other state
        // ECHO - prints what you type
        // ICANON - canonical mode, reads input line by line. Disabling reads input byte-by-byte
        // IEXTEN - Ctrl-V, sometimes waits for you to type another character and then sends that character literally
        // ISIG - SIGINT (Ctrl+C) and SIGTSTP (Ctrl+Z)
        termios.local_flags &=
            !(LocalFlags::ECHO | LocalFlags::ICANON | LocalFlags::IEXTEN | LocalFlags::ISIG);

        // Modify control flags
        // CS8 - bit mask that sets the character size to 8 bits per byte. Probably already set this way.
        termios.control_flags |= ControlFlags::CS8;

        // Modify control characters
        // VMIN - min number of bytes before read() can return
        // VTIME - max amount of tie to wait before read() returns, in tenths of a second
        termios.control_chars[VMIN] = 0;
        termios.control_chars[VTIME] = 1;
        // Apply the changes to the terminal. Use TCSAFLUSH to apply them after any pending output is written
        // and discard any unread input.
        tcsetattr(self.stdin_fd, SetArg::TCSAFLUSH, &termios)?;
        Ok(())
    }

    fn disable_raw_mode(&self) -> Result<(), Errno> {
        // The supplied termios struct should contain the original terminal attributes before any modifications

        tcsetattr(self.stdin_fd, SetArg::TCSAFLUSH, &self.orig_termios)
    }
}
