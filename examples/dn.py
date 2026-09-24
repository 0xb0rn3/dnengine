"""ctypes binding example. python3 examples/dn.py URL DEST [URL DEST ...]

fetch_many calls the stable convenience ABI. Client.batch returns individual
results, and Client.bytes can feed zipfile.ZipFile(io.BytesIO(...)) directly.
"""
import ctypes as C
import os
import sys

lib = C.CDLL(os.environ.get("DNENGINE_LIB", "libdnengine.so"))
PROGRESS = C.CFUNCTYPE(C.c_int, C.c_uint64, C.c_uint64, C.c_uint64, C.c_int, C.c_void_p)


class Options(C.Structure):
    _fields_ = [(n, C.c_uint32) for n in (
        "struct_size", "parallel", "per_host", "retries", "backoff_ms",
        "max_backoff_ms", "connect_timeout_ms", "timeout_ms", "total_timeout_ms")]
    _fields_ += [("ca_file", C.c_char_p)]


class Result(C.Structure):
    _fields_ = [("index", C.c_uint64), ("status", C.c_int), ("http_status", C.c_int),
                ("transport_code", C.c_int), ("attempts", C.c_uint32),
                ("bytes", C.c_uint64), ("error", C.c_char * 256)]


def bind(name, args, result):
    f = getattr(lib, name)
    f.argtypes, f.restype = args, result
    return f


strings = C.POINTER(C.c_char_p)
bind("dn_last_error", [], C.c_char_p)
bind("dn_fetch_many", [strings, strings, C.c_int, C.c_int, PROGRESS, C.c_void_p], C.c_int)
bind("dn_client_options_init", [C.POINTER(Options)], None)
bind("dn_client_new", [C.POINTER(Options)], C.c_void_p)
bind("dn_client_free", [C.c_void_p], None)
bind("dn_fetch_many_ex", [C.c_void_p, strings, strings, C.c_size_t, C.POINTER(Result),
                           C.c_void_p, PROGRESS, C.c_void_p], C.c_int)
bind("dn_fetch_bytes", [C.c_void_p, C.c_char_p, C.c_size_t, C.POINTER(C.c_void_p),
                         C.POINTER(C.c_size_t), C.POINTER(Result), C.c_void_p], C.c_int)
bind("dn_buffer_free", [C.c_void_p, C.c_size_t], None)
bind("dn_cancel_new", [], C.c_void_p)
bind("dn_cancel", [C.c_void_p], None)
bind("dn_cancel_free", [C.c_void_p], None)


def arrays(items):
    items = list(items)
    return ((C.c_char_p * len(items))(*(str(u).encode() for u, _ in items)),
            (C.c_char_p * len(items))(*(os.fspath(d).encode() for _, d in items)))


def fetch_many(items, parallel=8):
    urls, dests = arrays(items)
    count = lib.dn_fetch_many(urls, dests, len(urls), parallel, PROGRESS(), None)
    if count < 0:
        raise RuntimeError(lib.dn_last_error().decode())
    return count


class Client:
    def __init__(self, per_host=4, parallel=8):
        opts = Options()
        lib.dn_client_options_init(C.byref(opts))
        opts.per_host, opts.parallel = per_host, parallel
        self.ptr = lib.dn_client_new(C.byref(opts))
        if not self.ptr:
            raise RuntimeError(lib.dn_last_error().decode())

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    def close(self):
        if self.ptr:
            lib.dn_client_free(self.ptr)
            self.ptr = None

    def batch(self, items, cancel=None):
        urls, dests = arrays(items)
        results = (Result * len(urls))()
        rc = lib.dn_fetch_many_ex(self.ptr, urls, dests, len(urls), results, cancel, PROGRESS(), None)
        if rc:
            raise RuntimeError(lib.dn_last_error().decode())
        return list(results)

    def bytes(self, url, max_bytes=16 * 1024 * 1024, cancel=None):
        data, size, result = C.c_void_p(), C.c_size_t(), Result()
        rc = lib.dn_fetch_bytes(self.ptr, url.encode(), max_bytes, C.byref(data),
                               C.byref(size), C.byref(result), cancel)
        if rc:
            raise RuntimeError(lib.dn_last_error().decode())
        try:
            return C.string_at(data, size.value)
        finally:
            lib.dn_buffer_free(data, size)


if __name__ == "__main__":
    if len(sys.argv) < 3 or len(sys.argv) % 2 != 1:
        sys.exit(f"usage: {sys.argv[0]} URL DEST [URL DEST ...]")
    jobs = list(zip(sys.argv[1::2], sys.argv[2::2]))
    succeeded = fetch_many(jobs)
    print(f"batch: {succeeded}/{len(jobs)} succeeded")
    sys.exit(0 if succeeded == len(jobs) else 1)
