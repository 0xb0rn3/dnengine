"""Using the engine from Python, through the same C ABI that C uses.

    python3 examples/dn.py https://host/file.iso /tmp/file.iso [sha256]

No Python package to install and no HTTP code here: the engine does the work, so a Python tool
gets multi connection, multi interface, resumable, verified downloads for the price of a ctypes
call. Any language with a C FFI binds it the same way.
"""
import ctypes, sys, os

LIB = os.environ.get("DNENGINE_LIB", "libdnengine.so")
dn = ctypes.CDLL(LIB)

PROGRESS = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_uint64, ctypes.c_uint64,
                            ctypes.c_uint64, ctypes.c_int, ctypes.c_void_p)


class Options(ctypes.Structure):
    _fields_ = [("streams_per_lane", ctypes.c_int),
                ("max_connections", ctypes.c_int),
                ("expect_sha256", ctypes.c_char_p),
                ("interfaces", ctypes.c_char_p),
                ("no_resume", ctypes.c_int)]


dn.dn_download.argtypes = [ctypes.POINTER(ctypes.c_char_p), ctypes.c_int, ctypes.c_char_p,
                           ctypes.POINTER(Options), PROGRESS, ctypes.c_void_p]
dn.dn_last_error.restype = ctypes.c_char_p
dn.dn_version.restype = ctypes.c_char_p


def download(urls, dest, expect=None, streams=4):
    def show(done, total, bps, conns, _user):
        if total:
            pct = done * 100 // total
            print(f"\r  {pct:3d}%  {done:,} / {total:,} bytes  {bps/1e6:.1f} MB/s  {conns} conn ",
                  end="", flush=True)
        return 0

    arr = (ctypes.c_char_p * len(urls))(*[u.encode() for u in urls])
    opts = Options(streams, 0, expect.encode() if expect else None, None, 0)
    rc = dn.dn_download(arr, len(urls), dest.encode(), ctypes.byref(opts), PROGRESS(show), None)
    print()
    if rc != 0:
        raise RuntimeError(dn.dn_last_error().decode())
    return dest


if __name__ == "__main__":
    if len(sys.argv) < 3:
        sys.exit(f"usage: {sys.argv[0]} <url> <dest> [sha256]")
    print(dn.dn_version().decode())
    download([sys.argv[1]], sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else None)
    print("  ok", sys.argv[2])
