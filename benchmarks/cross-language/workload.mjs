function checksum() {
  const prime = 1000000007n;
  let numeric = 17n;
  let graph = 29n;
  let text = 7n;
  for (let index = 0n; index < 500000n; index += 1n) {
    numeric = (numeric * 1664525n + index * 1013904223n + 12345n) % prime;
    graph = (graph + ((index * 31n + 17n) % 9973n) * ((index % 13n) + 1n)) % prime;
    text = (text + [84n, 78n, 67n, 72n][Number(index % 4n)]) % prime;
  }
  return (numeric + graph * 31n + text * 131n) % prime;
}

if (checksum() !== 899120682n) {
  console.log("checksum=invalid");
  process.exitCode = 1;
} else {
  console.log("checksum=899120682");
}
