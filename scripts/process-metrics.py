#!/usr/bin/env python3
"""Read CPU, memory, scheduling, and syscall counters for a macOS process."""

from __future__ import annotations

import ctypes
import json
import sys


PROC_PIDTASKINFO = 4
TIME_FIELDS = {
    "total_user_nanoseconds",
    "total_system_nanoseconds",
    "threads_user_nanoseconds",
    "threads_system_nanoseconds",
}


class MachTimebaseInfo(ctypes.Structure):
    _fields_ = [("numerator", ctypes.c_uint32), ("denominator", ctypes.c_uint32)]


class ProcessTaskInfo(ctypes.Structure):
    _fields_ = [
        ("virtual_size", ctypes.c_uint64),
        ("resident_size", ctypes.c_uint64),
        ("total_user_nanoseconds", ctypes.c_uint64),
        ("total_system_nanoseconds", ctypes.c_uint64),
        ("threads_user_nanoseconds", ctypes.c_uint64),
        ("threads_system_nanoseconds", ctypes.c_uint64),
        ("policy", ctypes.c_int32),
        ("faults", ctypes.c_int32),
        ("pageins", ctypes.c_int32),
        ("copy_on_write_faults", ctypes.c_int32),
        ("messages_sent", ctypes.c_int32),
        ("messages_received", ctypes.c_int32),
        ("mach_syscalls", ctypes.c_int32),
        ("unix_syscalls", ctypes.c_int32),
        ("context_switches", ctypes.c_int32),
        ("thread_count", ctypes.c_int32),
        ("running_thread_count", ctypes.c_int32),
        ("priority", ctypes.c_int32),
    ]


def process_metrics(pid: int) -> dict[str, int]:
    library = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    library.proc_pidinfo.argtypes = [
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_uint64,
        ctypes.c_void_p,
        ctypes.c_int,
    ]
    library.proc_pidinfo.restype = ctypes.c_int
    info = ProcessTaskInfo()
    size = ctypes.sizeof(info)
    result = library.proc_pidinfo(
        pid, PROC_PIDTASKINFO, 0, ctypes.byref(info), size
    )
    if result != size:
        error = ctypes.get_errno()
        raise OSError(error, f"proc_pidinfo returned {result} of {size} bytes")
    timebase = MachTimebaseInfo()
    system = ctypes.CDLL(None)
    system.mach_timebase_info.argtypes = [ctypes.POINTER(MachTimebaseInfo)]
    system.mach_timebase_info.restype = ctypes.c_int
    if system.mach_timebase_info(ctypes.byref(timebase)) != 0:
        raise OSError("mach_timebase_info failed")
    result_metrics = {}
    for name, _field_type in ProcessTaskInfo._fields_:
        value = int(getattr(info, name))
        if name in TIME_FIELDS:
            value = value * timebase.numerator // timebase.denominator
        result_metrics[name] = value
    return result_metrics


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} PID", file=sys.stderr)
        return 2
    print(json.dumps(process_metrics(int(sys.argv[1])), sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
