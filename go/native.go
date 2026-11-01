package traverse

import (
	"fmt"
	"os"
	"path/filepath"
	"sync"
)

// nativeFn is a function exported by the native library, bound by loadLibrary.
type nativeFn struct{ addr uintptr }

// Call calls the function with integer and pointer arguments and returns its integer result. A
// float result comes back in a register Call doesn't read, so floats are read as strings. Memory
// passed by address must stay alive until Call returns (runtime.KeepAlive); go:uintptrescapes
// does that for pointers converted in the call's own argument list.
//
//go:uintptrescapes
func (f nativeFn) Call(args ...uintptr) uintptr {
	return syscallN(f.addr, args...)
}

var (
	libOnce sync.Once
	libErr  error
)

// loadLibrary loads and binds the native library once, from TRAVERSE_FFI_LIB, else a build in this
// source tree's target directory, else next to the executable, else by name from the system's
// library search path.
func loadLibrary() error {
	libOnce.Do(func() { libErr = bind(findLibrary()) })
	return libErr
}

func findLibrary() string {
	if p := os.Getenv("TRAVERSE_FFI_LIB"); p != "" {
		return p
	}
	name := libraryName()
	candidates := []string{
		filepath.Join("..", "target", "debug", name),
		filepath.Join("..", "target", "release", name),
	}
	if exe, err := os.Executable(); err == nil {
		dir := filepath.Dir(exe)
		candidates = append(candidates,
			filepath.Join(dir, name),
			filepath.Join(dir, "..", "target", "debug", name),
			filepath.Join(dir, "..", "target", "release", name),
		)
	}
	for _, c := range candidates {
		if abs, err := filepath.Abs(c); err == nil {
			if _, err := os.Stat(abs); err == nil {
				return abs
			}
		}
	}
	return name
}

func bind(path string) error {
	lib, err := openLibrary(path)
	if err != nil {
		return fmt.Errorf("traverse: loading %s: %w", path, err)
	}
	for _, e := range exports {
		addr, err := librarySymbol(lib, e.name)
		if err != nil {
			return fmt.Errorf("traverse: %s has no %s (a library from another version?): %w", path, e.name, err)
		}
		e.fn.addr = addr
	}
	return nil
}
