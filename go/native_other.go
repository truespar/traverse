//go:build !windows && !darwin && !linux

package traverse

import (
	"errors"
	"runtime"
)

var errUnsupported = errors.New("traverse: the embedded binding doesn't run on " + runtime.GOOS)

func libraryName() string { return "libtraverse_ffi.so" }

func openLibrary(string) (uintptr, error) { return 0, errUnsupported }

func librarySymbol(uintptr, string) (uintptr, error) { return 0, errUnsupported }

func syscallN(uintptr, ...uintptr) uintptr { panic(errUnsupported) }
