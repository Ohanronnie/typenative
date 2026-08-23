use std::collections::HashMap;
use std::env;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, RwLock};
use std::thread;

const MAXIMUM_BULK_LENGTH: usize = 536_870_912;
const MAXIMUM_PARTS: usize = 1_024;
const MAXIMUM_BATCH: usize = 1_024;
const READ_CAPACITY: usize = 4 * 1_024;

type Database = Arc<RwLock<HashMap<Vec<u8>, Vec<u8>>>>;

#[derive(Clone, Copy)]
struct ParsedCommand {
    consumed: usize,
    count: usize,
    parts: [(usize, usize); 3],
}

enum ParseResult {
    Complete(ParsedCommand),
    Incomplete,
    Invalid,
}

fn line_end(input: &[u8], start: usize) -> Option<usize> {
    input
        .get(start..)?
        .windows(2)
        .position(|bytes| bytes == b"\r\n")
        .map(|offset| start + offset)
}

fn unsigned_integer(input: &[u8]) -> Option<usize> {
    if input.is_empty() {
        return None;
    }
    input.iter().try_fold(0usize, |value, byte| {
        if !byte.is_ascii_digit() {
            return None;
        }
        value
            .checked_mul(10)?
            .checked_add(usize::from(*byte - b'0'))
    })
}

fn parse_command(input: &[u8], start: usize) -> ParseResult {
    if start == input.len() {
        return ParseResult::Incomplete;
    }
    if input[start] != b'*' {
        return ParseResult::Invalid;
    }
    let Some(count_end) = line_end(input, start + 1) else {
        return ParseResult::Incomplete;
    };
    let Some(count) = unsigned_integer(&input[start + 1..count_end]) else {
        return ParseResult::Invalid;
    };
    if count == 0 || count > MAXIMUM_PARTS {
        return ParseResult::Invalid;
    }

    let mut offset = count_end + 2;
    let mut parts = [(0, 0); 3];
    for index in 0..count {
        if offset == input.len() {
            return ParseResult::Incomplete;
        }
        if input[offset] != b'$' {
            return ParseResult::Invalid;
        }
        let Some(length_end) = line_end(input, offset + 1) else {
            return ParseResult::Incomplete;
        };
        let Some(length) = unsigned_integer(&input[offset + 1..length_end]) else {
            return ParseResult::Invalid;
        };
        if length > MAXIMUM_BULK_LENGTH {
            return ParseResult::Invalid;
        }
        let payload_start = length_end + 2;
        let Some(payload_end) = payload_start.checked_add(length) else {
            return ParseResult::Invalid;
        };
        let Some(frame_end) = payload_end.checked_add(2) else {
            return ParseResult::Invalid;
        };
        if frame_end > input.len() {
            return ParseResult::Incomplete;
        }
        if &input[payload_end..frame_end] != b"\r\n"
            || std::str::from_utf8(&input[payload_start..payload_end]).is_err()
        {
            return ParseResult::Invalid;
        }
        if index < parts.len() {
            parts[index] = (payload_start, payload_end);
        }
        offset = frame_end;
    }

    ParseResult::Complete(ParsedCommand {
        consumed: offset,
        count,
        parts,
    })
}

fn part<'a>(input: &'a [u8], parsed: &ParsedCommand, index: usize) -> &'a [u8] {
    let (start, end) = parsed.parts[index];
    &input[start..end]
}

fn append_decimal(output: &mut Vec<u8>, mut value: usize) {
    let mut digits = [0u8; 20];
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    output.extend_from_slice(&digits[start..]);
}

fn execute(input: &[u8], parsed: &ParsedCommand, database: &Database, output: &mut Vec<u8>) {
    let command = part(input, parsed, 0);
    if command.eq_ignore_ascii_case(b"PING") {
        output.extend_from_slice(b"+PONG\r\n");
        return;
    }
    if command.eq_ignore_ascii_case(b"SET") {
        if parsed.count < 3 {
            output.extend_from_slice(b"-ERR SET requires a key and value\r\n");
            return;
        }
        let key = part(input, parsed, 1).to_vec();
        let value = part(input, parsed, 2).to_vec();
        database
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, value);
        output.extend_from_slice(b"+OK\r\n");
        return;
    }
    if command.eq_ignore_ascii_case(b"GET") {
        if parsed.count < 2 {
            output.extend_from_slice(b"-ERR GET requires a key\r\n");
            return;
        }
        let database = database
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(value) = database.get(part(input, parsed, 1)) {
            output.push(b'$');
            append_decimal(output, value.len());
            output.extend_from_slice(b"\r\n");
            output.extend_from_slice(value);
            output.extend_from_slice(b"\r\n");
        } else {
            output.extend_from_slice(b"$-1\r\n");
        }
        return;
    }
    if command.eq_ignore_ascii_case(b"DEL") {
        if parsed.count < 2 {
            output.extend_from_slice(b"-ERR DEL requires a key\r\n");
            return;
        }
        let removed = database
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(part(input, parsed, 1))
            .is_some();
        output.extend_from_slice(if removed { b":1\r\n" } else { b":0\r\n" });
        return;
    }
    output.extend_from_slice(b"-ERR unknown command\r\n");
}

fn serve_connection(mut stream: TcpStream, database: &Database) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let mut input = Vec::with_capacity(READ_CAPACITY);
    let mut output = Vec::with_capacity(8 * 1_024);
    let mut read_buffer = [0u8; READ_CAPACITY];

    loop {
        let count = stream.read(&mut read_buffer)?;
        if count == 0 {
            return Ok(());
        }
        input.extend_from_slice(&read_buffer[..count]);
        let mut consumed = 0;

        while consumed < input.len() {
            output.clear();
            let mut commands = 0;
            let mut incomplete = false;
            while commands < MAXIMUM_BATCH && consumed < input.len() {
                match parse_command(&input, consumed) {
                    ParseResult::Complete(parsed) => {
                        execute(&input, &parsed, database, &mut output);
                        consumed = parsed.consumed;
                        commands += 1;
                    }
                    ParseResult::Incomplete => {
                        incomplete = true;
                        break;
                    }
                    ParseResult::Invalid => return Ok(()),
                }
            }
            if !output.is_empty() {
                stream.write_all(&output)?;
            }
            if incomplete {
                break;
            }
        }

        if consumed == input.len() {
            input.clear();
        } else if consumed > 0 {
            input.copy_within(consumed.., 0);
            input.truncate(input.len() - consumed);
        }
    }
}

fn main() -> io::Result<()> {
    let port = env::var("REDIS_RUST_PORT")
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
        .parse::<u16>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let database = Arc::new(RwLock::new(HashMap::new()));
    for stream in listener.incoming() {
        let stream = stream?;
        let connection_database = Arc::clone(&database);
        thread::spawn(move || {
            let _ = serve_connection(stream, &connection_database);
        });
    }
    Ok(())
}
