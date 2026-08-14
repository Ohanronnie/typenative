const SPACE = 32;
const QUOTE = 34;
const NEWLINE = 10;

function isDigit(byte) {
  return byte >= 48 && byte <= 57;
}

export function analyzeMany(input, iterations) {
  let allIterations = 0;

  for (let iteration = 0; iteration < iterations; iteration += 1) {
    let offset = 0;
    let records = 0;
    let informational = 0;
    let successful = 0;
    let redirects = 0;
    let clientErrors = 0;
    let serverErrors = 0;
    let totalBytes = 0;
    let totalDuration = 0;
    let methodChecksum = 0;
    let prefixChecksum = 0;
    let routeAggregate = 0;
    let busiestRoute = 0;
    let busiestCount = 0;
    const routes = new Map();

    while (offset < input.length) {
      let prefixLength = 0;
      while (offset < input.length && input[offset] !== QUOTE) {
        const byte = input[offset];
        if (byte === NEWLINE) return 0;
        prefixLength += 1;
        offset += 1;
      }
      if (offset >= input.length || prefixLength < 20) return 0;
      prefixChecksum = (prefixChecksum + prefixLength) % 2_147_483_629;
      offset += 1;

      const methodStart = offset;
      while (offset < input.length && input[offset] !== SPACE) offset += 1;
      if (offset >= input.length || offset === methodStart) return 0;
      const methodLength = offset - methodStart;
      const methodFirst = input[methodStart];
      if (!(
        (methodFirst === 71 && methodLength === 3) ||
        (methodFirst === 80 && methodLength === 3) ||
        (methodFirst === 80 && methodLength === 4) ||
        (methodFirst === 80 && methodLength === 5) ||
        (methodFirst === 68 && methodLength === 6)
      )) {
        return 0;
      }
      if (methodFirst === 71) methodChecksum += 3;
      else if (methodLength === 3) methodChecksum += 13;
      else if (methodLength === 4) methodChecksum += 5;
      else if (methodLength === 5) methodChecksum += 7;
      else methodChecksum += 11;
      offset += 1;

      const routeStart = offset;
      let routeHash = 0;
      while (offset < input.length && input[offset] !== SPACE) {
        const byte = input[offset];
        routeHash = (routeHash * 131 + byte) % 2_147_483_629;
        offset += 1;
      }
      if (offset >= input.length || offset === routeStart) return 0;
      offset += 1;

      const protocolStart = offset;
      while (offset < input.length && input[offset] !== QUOTE) offset += 1;
      if (offset >= input.length || offset - protocolStart < 8) return 0;
      offset += 1;
      if (offset >= input.length || input[offset] !== SPACE) return 0;
      offset += 1;

      let status = 0;
      for (let digit = 0; digit < 3; digit += 1) {
        if (offset >= input.length || !isDigit(input[offset])) return 0;
        status = status * 10 + input[offset] - 48;
        offset += 1;
      }
      if (offset >= input.length || input[offset] !== SPACE) return 0;
      offset += 1;

      let byteCount = 0;
      const bytesStart = offset;
      while (offset < input.length && isDigit(input[offset])) {
        byteCount = byteCount * 10 + input[offset] - 48;
        offset += 1;
      }
      if (
        offset === bytesStart ||
        offset >= input.length ||
        input[offset] !== SPACE
      )
        return 0;
      offset += 1;

      let duration = 0;
      const durationStart = offset;
      while (offset < input.length && isDigit(input[offset])) {
        duration = duration * 10 + input[offset] - 48;
        offset += 1;
      }
      if (offset === durationStart) return 0;
      if (offset < input.length && input[offset] === NEWLINE) offset += 1;
      else if (offset !== input.length) return 0;

      if (status >= 100 && status < 200) informational += 1;
      else if (status < 300) successful += 1;
      else if (status < 400) redirects += 1;
      else if (status < 500) clientErrors += 1;
      else if (status < 600) serverErrors += 1;
      else return 0;

      const routeCount = (routes.get(routeHash) ?? 0) + 1;
      routes.set(routeHash, routeCount);
      routeAggregate = (routeAggregate + routeHash) % 9_007_199_254_740_881;
      if (
        routeCount > busiestCount ||
        (routeCount === busiestCount && routeHash < busiestRoute)
      ) {
        busiestCount = routeCount;
        busiestRoute = routeHash;
      }
      records += 1;
      totalBytes += byteCount;
      totalDuration += duration;
    }

    if (records === 0 || routes.size === 0) return 0;
    const checksum =
      records * 3 +
      informational * 5 +
      successful * 7 +
      redirects * 11 +
      clientErrors * 13 +
      serverErrors * 17 +
      totalBytes * 19 +
      totalDuration * 23 +
      methodChecksum * 29 +
      prefixChecksum * 31 +
      routeAggregate +
      busiestRoute * 37 +
      busiestCount * 41 +
      routes.size * 43;
    allIterations += checksum;
  }

  return allIterations;
}
