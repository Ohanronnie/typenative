const std = @import("std");

fn checksum() u64 {
    const prime: u64 = 1000000007;
    var numeric: u64 = 17;
    var graph: u64 = 29;
    var text: u64 = 7;
    var index: u64 = 0;
    while (index < 500000) : (index += 1) {
        numeric = (numeric * 1664525 + index * 1013904223 + 12345) % prime;
        graph = (graph + ((index * 31 + 17) % 9973) * ((index % 13) + 1)) % prime;
        const lane = index % 4;
        text = (text + switch (lane) {
            0 => 84,
            1 => 78,
            2 => 67,
            else => 72,
        }) % prime;
    }
    return (numeric + graph * 31 + text * 131) % prime;
}

pub fn main() !void {
    if (checksum() != 899120682) return error.InvalidChecksum;
    try std.io.getStdOut().writer().print("checksum=899120682\n", .{});
}
