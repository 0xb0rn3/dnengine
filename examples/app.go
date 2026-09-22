// Using the engine from Go, through the C ABI.
//
//	CGO_LDFLAGS="-L../target/release -ldnengine" go run app.go https://host/file.iso /tmp/file.iso
//
// Go could shell out to `dn` instead, and for a one-off that is simpler. Linking is what you
// want when the progress belongs in your own UI.
package main

/*
#cgo LDFLAGS: -ldnengine
#include <stdlib.h>
#include "dnengine.h"
extern int goProgress(unsigned long long done, unsigned long long total,
                      unsigned long long bps, int conns, void *user);
*/
import "C"

import (
	"fmt"
	"os"
	"unsafe"
)

//export goProgress
func goProgress(done, total, bps C.ulonglong, conns C.int, user unsafe.Pointer) C.int {
	if total > 0 {
		fmt.Printf("\r  %d%%  %d / %d bytes  %d conn ", done*100/total, done, total, conns)
	}
	return 0 // non-zero cancels
}

func main() {
	if len(os.Args) < 3 {
		fmt.Fprintln(os.Stderr, "usage: app <url> <dest>")
		os.Exit(2)
	}
	url := C.CString(os.Args[1])
	dest := C.CString(os.Args[2])
	defer C.free(unsafe.Pointer(url))
	defer C.free(unsafe.Pointer(dest))

	urls := (*C.char)(url)
	opts := C.dn_options{streams_per_lane: 4}
	rc := C.dn_download(&urls, 1, dest, &opts, C.dn_progress_fn(C.goProgress), nil)
	fmt.Println()
	if rc != C.DN_OK {
		fmt.Fprintf(os.Stderr, "  failed: %s\n", C.GoString(C.dn_last_error()))
		os.Exit(1)
	}
	fmt.Println("  ok", os.Args[2])
}
