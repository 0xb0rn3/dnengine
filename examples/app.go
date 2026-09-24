// Cgo example: all Go pointers stay in Go. String arrays and strings are C-owned.
// Build on the host with the commands in BUILD.md; run in tests/vm.py.
package main

/*
#cgo CFLAGS: -I${SRCDIR}/../include
#cgo LDFLAGS: -ldnengine
#include <stdlib.h>
#include "dnengine.h"
*/
import "C"

import (
    "fmt"
    "os"
    "runtime"
    "unsafe"
)

func fetchMany(args []string) (int, error) {
    n := len(args) / 2
    urlsMem := C.calloc(C.size_t(n), C.size_t(unsafe.Sizeof(uintptr(0))))
    destsMem := C.calloc(C.size_t(n), C.size_t(unsafe.Sizeof(uintptr(0))))
    if urlsMem == nil || destsMem == nil {
        C.free(urlsMem)
        C.free(destsMem)
        return 0, fmt.Errorf("cannot allocate batch arrays")
    }
    defer C.free(urlsMem)
    defer C.free(destsMem)
    urls := unsafe.Slice((**C.char)(urlsMem), n)
    dests := unsafe.Slice((**C.char)(destsMem), n)
    for i := 0; i < n; i++ {
        urls[i], dests[i] = C.CString(args[i*2]), C.CString(args[i*2+1])
        defer C.free(unsafe.Pointer(urls[i]))
        defer C.free(unsafe.Pointer(dests[i]))
    }
    // dn_last_error is OS-thread-local. Pin both FFI calls to the same thread.
    runtime.LockOSThread()
    defer runtime.UnlockOSThread()
    rc := C.dn_fetch_many((**C.char)(urlsMem), (**C.char)(destsMem), C.int(n), 8, nil, nil)
    if rc < 0 {
        return 0, fmt.Errorf("%s", C.GoString(C.dn_last_error()))
    }
    return int(rc), nil
}

func main() {
    if len(os.Args) < 3 || len(os.Args)%2 != 1 {
        fmt.Fprintln(os.Stderr, "usage: app URL DEST [URL DEST ...]")
        os.Exit(2)
    }
    n, err := fetchMany(os.Args[1:])
    if err != nil {
        fmt.Fprintln(os.Stderr, err)
        os.Exit(1)
    }
    fmt.Printf("batch: %d/%d succeeded\n", n, (len(os.Args)-1)/2)
    if n != (len(os.Args)-1)/2 { os.Exit(1) }
}
