use std::collections::HashMap;
use std::env;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

const MAXIMUM_BULK_LENGTH: usize = 536_870_912;
const MAXIMUM_PARTS: usize = 1_024;
const MAXIMUM_BATCH: usize = 1_024;
const READ_CAPACITY: usize = 4 * 1_024;

struct DatabaseState {
    values: HashMap<Vec<u8>, Vec<u8>>,
    expirations: HashMap<Vec<u8>, Instant>,
}

type Database = Arc<RwLock<DatabaseState>>;

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

fn append_bulk(output: &mut Vec<u8>, value: &[u8]) {
    output.push(b'$');
    append_decimal(output, value.len());
    output.extend_from_slice(b"\r\n");
    output.extend_from_slice(value);
    output.extend_from_slice(b"\r\n");
}

fn append_integer(output: &mut Vec<u8>, value: usize) {
    output.push(b':');
    append_decimal(output, value);
    output.extend_from_slice(b"\r\n");
}

fn purge_expired(state: &mut DatabaseState, key: &[u8]) {
    let expired = state
        .expirations
        .get(key)
        .is_some_and(|deadline| Instant::now() >= *deadline);
    if expired {
        state.values.remove(key);
        state.expirations.remove(key);
    }
}

fn execute(
    input: &[u8],
    parsed: &ParsedCommand,
    database: &Database,
    output: &mut Vec<u8>,
) -> bool {
    let command = part(input, parsed, 0);
    if command.eq_ignore_ascii_case(b"ECHO") {
        if parsed.count < 2 {
            output.extend_from_slice(b"-ERR ECHO requires a message\r\n");
        } else {
            append_bulk(output, part(input, parsed, 1));
        }
        return false;
    }
    if command.eq_ignore_ascii_case(b"PING") {
        if parsed.count >= 2 {
            append_bulk(output, part(input, parsed, 1));
        } else {
            output.extend_from_slice(b"+PONG\r\n");
        }
        return false;
    }
    if command.eq_ignore_ascii_case(b"SET") {
        if parsed.count < 3 {
            output.extend_from_slice(b"-ERR SET requires a key and value\r\n");
            return false;
        }
        let key = part(input, parsed, 1).to_vec();
        let value = part(input, parsed, 2).to_vec();
        let mut state = database
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.values.insert(key.clone(), value);
        state.expirations.remove(&key);
        output.extend_from_slice(b"+OK\r\n");
        return false;
    }
    if command.eq_ignore_ascii_case(b"GET") {
        if parsed.count < 2 {
            output.extend_from_slice(b"-ERR GET requires a key\r\n");
            return false;
        }
        let key = part(input, parsed, 1);
        let mut state = database
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        purge_expired(&mut state, key);
        if let Some(value) = state.values.get(key) {
            append_bulk(output, value);
        } else {
            output.extend_from_slice(b"$-1\r\n");
        }
        return false;
    }
    if command.eq_ignore_ascii_case(b"DEL") {
        if parsed.count < 2 {
            output.extend_from_slice(b"-ERR DEL requires a key\r\n");
            return false;
        }
        let mut state = database
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut removed = 0;
        for index in 1..parsed.count.min(3) {
            let key = part(input, parsed, index);
            purge_expired(&mut state, key);
            if state.values.remove(key).is_some() {
                removed += 1;
            }
            state.expirations.remove(key);
        }
        append_integer(output, removed);
        return false;
    }
    if command.eq_ignore_ascii_case(b"EXISTS") {
        let mut state = database
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut found = 0;
        for index in 1..parsed.count.min(3) {
            let key = part(input, parsed, index);
            purge_expired(&mut state, key);
            if state.values.contains_key(key) {
                found += 1;
            }
        }
        append_integer(output, found);
        return false;
    }
    if command.eq_ignore_ascii_case(b"INCR") {
        if parsed.count < 2 {
            output.extend_from_slice(b"-ERR INCR requires a key\r\n");
            return false;
        }
        let key = part(input, parsed, 1);
        let mut state = database
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        purge_expired(&mut state, key);
        let value = match state.values.get(key) {
            None => 0,
            Some(value) => match unsigned_integer(value) {
                Some(value) => value,
                None => {
                    output.extend_from_slice(b"-ERR value is not an integer\r\n");
                    return false;
                }
            },
        };
        let Some(next) = value.checked_add(1) else {
            output.extend_from_slice(b"-ERR increment or decrement would overflow\r\n");
            return false;
        };
        let mut encoded = Vec::with_capacity(20);
        append_decimal(&mut encoded, next);
        state.values.insert(key.to_vec(), encoded);
        state.expirations.remove(key);
        append_integer(output, next);
        return false;
    }
    if command.eq_ignore_ascii_case(b"EXPIRE") {
        if parsed.count < 3 {
            output.extend_from_slice(b"-ERR EXPIRE requires a key and seconds\r\n");
            return false;
        }
        let key = part(input, parsed, 1);
        let Some(seconds) = unsigned_integer(part(input, parsed, 2)) else {
            output.extend_from_slice(b"-ERR value is not an integer\r\n");
            return false;
        };
        let mut state = database
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        purge_expired(&mut state, key);
        if !state.values.contains_key(key) {
            output.extend_from_slice(b":0\r\n");
            return false;
        }
        let duration = Duration::from_secs(seconds as u64);
        state
            .expirations
            .insert(key.to_vec(), Instant::now() + duration);
        output.extend_from_slice(b":1\r\n");
        return false;
    }
    if command.eq_ignore_ascii_case(b"TTL") {
        if parsed.count < 2 {
            output.extend_from_slice(b"-ERR TTL requires a key\r\n");
            return false;
        }
        let key = part(input, parsed, 1);
        let mut state = database
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        purge_expired(&mut state, key);
        if !state.values.contains_key(key) {
            output.extend_from_slice(b":-2\r\n");
            return false;
        }
        let Some(deadline) = state.expirations.get(key).copied() else {
            output.extend_from_slice(b":-1\r\n");
            return false;
        };
        output.push(b':');
        append_decimal(
            output,
            deadline.saturating_duration_since(Instant::now()).as_secs() as usize,
        );
        output.extend_from_slice(b"\r\n");
        return false;
    }
    if command.eq_ignore_ascii_case(b"COMMAND") {
        output.extend_from_slice(b"*0\r\n");
        return false;
    }
    if command.eq_ignore_ascii_case(b"QUIT") {
        output.extend_from_slice(b"+OK\r\n");
        return true;
    }
    output.extend_from_slice(b"-ERR unknown command\r\n");
    false
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
            let mut should_close = false;
            while commands < MAXIMUM_BATCH && consumed < input.len() {
                match parse_command(&input, consumed) {
                    ParseResult::Complete(parsed) => {
                        should_close = execute(&input, &parsed, database, &mut output);
                        consumed = parsed.consumed;
                        commands += 1;
                        if should_close {
                            break;
                        }
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
            if should_close {
                return Ok(());
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
    let database = Arc::new(RwLock::new(DatabaseState {
        values: HashMap::new(),
        expirations: HashMap::new(),
    }));
    for stream in listener.incoming() {
        let stream = stream?;
        let connection_database = Arc::clone(&database);
        thread::spawn(move || {
            let _ = serve_connection(stream, &connection_database);
        });
    }
    Ok(())
}
