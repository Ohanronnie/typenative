#![no_main]

use libfuzzer_sys::fuzz_target;

const MAX_BULK: usize = 1 << 20;
const MAX_ARRAY: usize = 1024;
const MAX_DEPTH: usize = 32;

fn line<'a>(input: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    let start = *cursor;
    let end = input
        .get(start..)?
        .windows(2)
        .position(|pair| pair == b"\r\n")?
        + start;
    *cursor = end + 2;
    Some(&input[start..end])
}

fn number(value: &[u8]) -> Option<i64> {
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn frame(input: &[u8], cursor: &mut usize, depth: usize) -> Option<()> {
    if depth > MAX_DEPTH {
        return None;
    }
    let kind = *input.get(*cursor)?;
    *cursor += 1;
    match kind {
        b'+' | b'-' | b':' => {
            let _ = line(input, cursor)?;
        }
        b'$' => {
            let length = number(line(input, cursor)?)?;
            if length == -1 {
                return Some(());
            }
            let length = usize::try_from(length).ok()?;
            let frame_length = length.checked_add(2)?;
            if length > MAX_BULK || input.len().checked_sub(*cursor)? < frame_length {
                return None;
            }
            *cursor += length;
            if input.get(*cursor..*cursor + 2)? != b"\r\n" {
                return None;
            }
            *cursor += 2;
        }
        b'*' => {
            let count = number(line(input, cursor)?)?;
            if count == -1 {
                return Some(());
            }
            let count = usize::try_from(count).ok()?;
            if count > MAX_ARRAY {
                return None;
            }
            for _ in 0..count {
                frame(input, cursor, depth + 1)?;
            }
        }
        _ => return None,
    }
    Some(())
}

fuzz_target!(|bytes: &[u8]| {
    let mut cursor = 0;
    let _ = frame(bytes, &mut cursor, 0);
    assert!(cursor <= bytes.len());
});
