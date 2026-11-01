//go:build darwin || linux

package traverse

import (
	"runtime"

	"github.com/ebitengine/purego"
)

func libraryName() string {
	if runtime.GOOS == "darwin" || runtime.GOOS == "ios" {
		return "libtraverse_ffi.dylib"
	}
	return "libtraverse_ffi.so"
}

// dlopen without CGo: purego calls into the system's dynamic loader itself.
func openLibrary(path string) (uintptr, error) {
	return purego.Dlopen(path, purego.RTLD_NOW|purego.RTLD_LOCAL)
}

func librarySymbol(lib uintptr, name string) (uintptr, error) {
	return purego.Dlsym(lib, name)
}

func syscallN(fn uintptr, args ...uintptr) uintptr {
	r, _, _ := purego.SyscallN(fn, args...)
	return r
}
