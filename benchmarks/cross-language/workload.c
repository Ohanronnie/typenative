#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

static uint64_t checksum(void) {
    const uint64_t prime = 1000000007ULL;
    uint64_t numeric = 17;
    uint64_t graph = 29;
    uint64_t text = 7;
    for (uint64_t index = 0; index < 500000; ++index) {
        numeric = (numeric * 1664525ULL + index * 1013904223ULL + 12345ULL) % prime;
        graph = (graph + ((index * 31ULL + 17ULL) % 9973ULL) * ((index % 13ULL) + 1ULL)) % prime;
        static const uint64_t lanes[] = {84, 78, 67, 72};
        text = (text + lanes[index % 4]) % prime;
    }
    return (numeric + graph * 31ULL + text * 131ULL) % prime;
}

int main(void) {
    if (checksum() != 899120682ULL) {
        puts("checksum=invalid");
        return EXIT_FAILURE;
    }
    puts("checksum=899120682");
    return EXIT_SUCCESS;
}
