export function parseMany(input, iterations) {
  let total = 0;
  let offset = 0;
  let checksum = 0;

  const current = () => input[offset];
  const isDigit = (value) => value >= 48 && value <= 57;
  const isHexDigit = (value) =>
    isDigit(value) ||
    (value >= 65 && value <= 70) ||
    (value >= 97 && value <= 102);
  const skipWhitespace = () => {
    while (offset < input.length) {
      const value = current();
      if (value !== 32 && value !== 10 && value !== 13 && value !== 9) return;
      offset += 1;
    }
  };
  const consume = (expected) => {
    if (offset >= input.length || current() !== expected) return false;
    offset += 1;
    return true;
  };
  const parseString = () => {
    if (!consume(34)) return false;
    while (offset < input.length) {
      const value = current();
      offset += 1;
      if (value === 34) return true;
      if (value < 32) return false;
      checksum += 1;
      if (value === 92) {
        if (offset >= input.length) return false;
        const escaped = current();
        offset += 1;
        checksum += 1;
        if (escaped === 117) {
          for (let digits = 0; digits < 4; digits += 1) {
            if (offset >= input.length || !isHexDigit(current())) return false;
            checksum += 1;
            offset += 1;
          }
        } else if (![34, 92, 47, 98, 102, 110, 114, 116].includes(escaped)) {
          return false;
        }
      }
    }
    return false;
  };
  const parseLiteral = (text) => {
    if (offset + text.length > input.length) return false;
    for (let index = 0; index < text.length; index += 1) {
      const value = text.charCodeAt(index);
      if (input[offset + index] !== value) return false;
      checksum += 1;
    }
    offset += text.length;
    return true;
  };
  const parseNumber = () => {
    const start = offset;
    if (offset < input.length && current() === 45) offset += 1;
    if (offset >= input.length) return false;
    if (current() === 48) {
      checksum += 1;
      offset += 1;
    } else {
      if (current() < 49 || current() > 57) return false;
      while (offset < input.length && isDigit(current())) {
        checksum += 1;
        offset += 1;
      }
    }
    if (offset < input.length && current() === 46) {
      offset += 1;
      if (offset >= input.length || !isDigit(current())) return false;
      while (offset < input.length && isDigit(current())) {
        checksum += 1;
        offset += 1;
      }
    }
    if (offset < input.length && (current() === 101 || current() === 69)) {
      offset += 1;
      if (offset < input.length && (current() === 43 || current() === 45))
        offset += 1;
      if (offset >= input.length || !isDigit(current())) return false;
      while (offset < input.length && isDigit(current())) {
        checksum += 1;
        offset += 1;
      }
    }
    return offset > start;
  };
  let parseValue;
  const parseArray = () => {
    if (!consume(91)) return false;
    skipWhitespace();
    if (consume(93)) return true;
    while (true) {
      if (!parseValue()) return false;
      skipWhitespace();
      if (consume(93)) return true;
      if (!consume(44)) return false;
      skipWhitespace();
    }
  };
  const parseObject = () => {
    if (!consume(123)) return false;
    skipWhitespace();
    if (consume(125)) return true;
    while (true) {
      if (!parseString()) return false;
      skipWhitespace();
      if (!consume(58)) return false;
      skipWhitespace();
      if (!parseValue()) return false;
      skipWhitespace();
      if (consume(125)) return true;
      if (!consume(44)) return false;
      skipWhitespace();
    }
  };
  parseValue = () => {
    skipWhitespace();
    if (offset >= input.length) return false;
    const value = current();
    if (value === 34) return parseString();
    if (value === 123) return parseObject();
    if (value === 91) return parseArray();
    if (value === 116) return parseLiteral("true");
    if (value === 102) return parseLiteral("false");
    if (value === 110) return parseLiteral("null");
    return parseNumber();
  };

  for (let iteration = 0; iteration < iterations; iteration += 1) {
    offset = 0;
    checksum = 0;
    const valid = parseValue();
    skipWhitespace();
    if (!valid || offset !== input.length) return 0;
    total += checksum + offset;
  }

  return total;
}
