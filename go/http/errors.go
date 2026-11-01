package http

import "fmt"

// TraverseHttpError represents an error returned by the Traverse HTTP API.
type TraverseHttpError struct {
	Message    string
	StatusCode int
}

func (e *TraverseHttpError) Error() string {
	if e.StatusCode > 0 {
		return fmt.Sprintf("%s (HTTP %d)", e.Message, e.StatusCode)
	}
	return e.Message
}
